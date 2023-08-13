use std::fs::File;
use std::io::{BufRead, BufReader, Result};
use std::sync::Arc;

use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};

#[derive(Debug, Clone)]
pub struct Config {
    pub cert: String,
    pub privkey: String,
    pub chain: String,
    pub fullchain: String,
}

pub fn parse_config(file_path: &str) -> Result<Config> {
    let file = File::open(file_path)?;
    let reader = BufReader::new(file);

    let mut cert = String::new();
    let mut privkey = String::new();
    let mut chain = String::new();
    let mut fullchain = String::new();

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("cert =") {
            cert = line["cert =".len()..].trim().to_string();
        } else if line.starts_with("privkey =") {
            privkey = line["privkey =".len()..].trim().to_string();
        } else if line.starts_with("chain =") {
            chain = line["chain =".len()..].trim().to_string();
        } else if line.starts_with("fullchain =") {
            fullchain = line["fullchain =".len()..].trim().to_string();
        }
    }

    Ok(Config {
        cert,
        privkey,
        chain,
        fullchain,
    })
}

pub type Acceptor = tokio_rustls::TlsAcceptor;

pub fn acceptor(file_path: &str) -> Result<Acceptor> {
    let config = parse_config(file_path)?;

    println!("Reading TLS config at {config:?}");

    let key_file = File::open(&config.privkey)?;
    let mut key_reader = BufReader::new(key_file);
    let key_bytes = match rustls_pemfile::read_one(&mut key_reader)? {
        Some(rustls_pemfile::Item::RSAKey(bytes)) => bytes,
        Some(rustls_pemfile::Item::PKCS8Key(bytes)) => bytes,
        Some(rustls_pemfile::Item::ECKey(bytes)) => bytes,
        something_else => {
            eprintln!("TLS key was {something_else:?}");
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid TLS key",
            ));
        }
    };

    let chain_file = File::open(&config.fullchain)?;
    let mut chain_reader = BufReader::new(chain_file);
    let chain: Vec<_> = rustls_pemfile::certs(&mut chain_reader)?
        .into_iter()
        .map(Certificate)
        .collect();

    Ok(Arc::new(
        ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(chain, PrivateKey(key_bytes))
            .expect("TLS setup failed"),
    )
    .into())
}
