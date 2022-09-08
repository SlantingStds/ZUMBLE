use crate::client::Client;
use crate::error::MumbleError;
use crate::handler::MessageHandler;
use crate::proto::mumble::Version;
use crate::proto::MessageKind;
use crate::voice::{Clientbound, VoicePacket};
use crate::ServerState;
use actix_server::Server;
use actix_service::fn_service;
use byteorder::{ReadBytesExt, WriteBytesExt};
use bytes::BytesMut;
use std::io::Cursor;
use std::sync::Arc;
use tokio::io;
use tokio::io::ReadHalf;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, RwLock};
use tokio_rustls::{server::TlsStream, TlsAcceptor};

pub fn create_tcp_server(
    tcp_listener: TcpListener,
    acceptor: TlsAcceptor,
    server_version: Version,
    state: Arc<RwLock<ServerState>>,
) -> Server {
    Server::build()
        .listen(
            "mumble-tcp",
            tcp_listener.into_std().expect("cannot create tcp listener"),
            move || {
                let acceptor = acceptor.clone();
                let server_version = server_version.clone();
                let state = state.clone();

                fn_service(move |stream: TcpStream| {
                    let acceptor = acceptor.clone();
                    let server_version = server_version.clone();
                    let state = state.clone();

                    stream.set_nodelay(true).unwrap();

                    async move {
                        let mut stream = acceptor.accept(stream).await.map_err(|e| {
                            log::error!("accept error: {}", e);

                            e
                        })?;

                        let (version, authenticate, crypt_state) = Client::init(&mut stream, server_version).await.map_err(|e| {
                            log::error!("init client error: {}", e);

                            e
                        })?;

                        let (read, write) = io::split(stream);
                        let (tx, rx) = mpsc::channel(32);

                        let client = { state.write().await.add_client(version, authenticate, crypt_state, write, tx) };

                        log::info!("new client {} connected", client.read().await.authenticate.get_username());

                        match client_run(read, rx, state.clone(), client.clone()).await {
                            Ok(_) => (),
                            Err(MumbleError::Io(err)) => {
                                if err.kind() != io::ErrorKind::UnexpectedEof {
                                    log::error!("client error: {}", err);
                                }
                            }
                            Err(e) => log::error!("client error: {}", e),
                        }

                        log::info!("client {} disconnected", client.read().await.authenticate.get_username());

                        {
                            state.write().await.disconnect(client).await;
                        }

                        Ok::<(), MumbleError>(())
                    }
                })
            },
        )
        .expect("cannot create tcp server")
        .run()
}

pub async fn client_run(
    mut read: ReadHalf<TlsStream<TcpStream>>,
    mut receiver: Receiver<VoicePacket<Clientbound>>,
    state: Arc<RwLock<ServerState>>,
    client: Arc<RwLock<Client>>,
) -> Result<(), MumbleError> {
    if let Some(codec_version) = { state.read().await.check_codec().await } {
        client.read().await.send_message(MessageKind::CodecVersion, &codec_version).await?;
    }

    {
        let client_sync = client.read().await;

        client_sync.sync_client_and_channels(&state).await.map_err(|e| {
            log::error!("init client error: {}", e);

            e
        })?;
        client_sync.send_my_user_state().await?;
        client_sync.send_server_sync().await?;
        client_sync.send_server_config().await?;
    }

    loop {
        MessageHandler::handle(&mut read, &mut receiver, state.clone(), client.clone()).await?
    }
}

pub async fn create_udp_server(protocol_version: u32, socket: Arc<UdpSocket>, state: Arc<RwLock<ServerState>>) {
    loop {
        let mut buffer = BytesMut::zeroed(1024);
        let (size, addr) = socket.recv_from(&mut buffer).await.expect("cannot receive udp packet");
        buffer.resize(size, 0);

        let mut cursor = Cursor::new(&buffer[..size]);
        let kind = cursor.read_u32::<byteorder::BigEndian>().unwrap();

        if size == 12 && kind == 0 {
            let timestamp = cursor.read_u64::<byteorder::LittleEndian>().unwrap();

            let mut send = Cursor::new(vec![0u8; 24]);
            send.write_u32::<byteorder::BigEndian>(protocol_version).unwrap();
            send.write_u64::<byteorder::LittleEndian>(timestamp).unwrap();
            send.write_u32::<byteorder::BigEndian>(0).unwrap();
            send.write_u32::<byteorder::BigEndian>(250).unwrap();
            send.write_u32::<byteorder::BigEndian>(72000).unwrap();

            socket
                .send_to(send.get_ref().as_slice(), addr)
                .await
                .expect("cannot send udp packet");

            continue;
        }

        let client_opt = { state.read().await.get_client_by_socket(&addr) };

        let (client, packet) = match client_opt {
            Some(client) => {
                let decrypt_result = { client.read().await.crypt_state.write().await.decrypt(&mut buffer) };

                match decrypt_result {
                    Ok(p) => (client, p),
                    Err(err) => {
                        let late = { client.read().await.crypt_state.read().await.late };

                        if late > 100 {
                            log::error!(
                                "too many late for client {} udp decrypt error: {}, reset crypt setup",
                                client.read().await.authenticate.get_username(),
                                err
                            );

                            {
                                client.read().await.crypt_state.write().await.reset();
                            }

                            let crypt_setup = { client.read().await.crypt_state.read().await.get_crypt_setup() };

                            match client.read().await.send_message(MessageKind::CryptSetup, &crypt_setup).await {
                                Ok(_) => (),
                                Err(e) => {
                                    log::error!("send crypt setup error: {}", e);
                                }
                            }
                        }

                        log::warn!("client decrypt error: {}", err);

                        continue;
                    }
                }
            }
            None => {
                let (client_opt, packet_opt) = { state.read().await.find_client_for_packet(&mut buffer).await };

                match (client_opt, packet_opt) {
                    (Some(client), Some(packet)) => {
                        {
                            log::info!(
                                "UPD connected client {} on {}",
                                client.read().await.authenticate.get_username(),
                                addr
                            );
                        }

                        {
                            state.write().await.set_client_socket(client.clone(), addr).await;
                        }

                        (client, packet)
                    }
                    _ => {
                        log::error!("unknown client from address {}", addr);

                        continue;
                    }
                }
            }
        };

        let client_packet = packet.to_client_bound(client.read().await.session_id);

        match &client_packet {
            VoicePacket::Ping { .. } => {
                let mut dest = BytesMut::new();

                {
                    client.read().await.crypt_state.write().await.encrypt(&client_packet, &mut dest);
                }

                match socket.send_to(&dest.freeze()[..], addr).await {
                    Ok(_) => (),
                    Err(err) => {
                        log::error!("cannot send ping udp packet: {}", err);
                    }
                }
            }
            _ => match client.read().await.publisher.send(client_packet).await {
                Ok(_) => (),
                Err(err) => {
                    log::error!("cannot send voice packet to client: {}", err);
                }
            },
        };
    }
}