use lazy_static::lazy_static;
use prometheus::{opts, register_int_gauge};
use prometheus::IntGauge;

lazy_static! {
    pub static ref CLIENTS_TOTAL: IntGauge =
        register_int_gauge!(opts!("zumble_clients_total", "Total number of clients")).expect("can't create a metric");
}
