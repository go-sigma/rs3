use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "rs3", version, about)]
pub struct Config {
    /// Address to listen on
    #[arg(long, env = "RS3_HOST", default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    pub host: IpAddr,

    /// HTTP port to listen on
    #[arg(long, env = "RS3_PORT", default_value_t = 9000)]
    pub port: u16,

    /// Directory used for buckets, objects, and multipart uploads
    #[arg(long, env = "RS3_DATA_DIR", default_value = "data")]
    pub data_dir: PathBuf,

    /// AWS access key ID
    #[arg(long, env = "RS3_ACCESS_KEY", default_value = "minioadmin")]
    pub access_key: String,

    /// AWS secret access key
    #[arg(long, env = "RS3_SECRET_KEY", default_value = "minioadmin")]
    pub secret_key: String,
}

impl Config {
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    pub fn uses_default_credentials(&self) -> bool {
        self.access_key == "minioadmin" && self.secret_key == "minioadmin"
    }
}
