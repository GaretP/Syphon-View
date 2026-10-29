//! Used by tests/vpn_e2e.sh: does what the app does with a VPN code, from the command line.
//!   vpn_check enroll <code> <own-key-file>   first contact
//!   vpn_check get <code> <own-key-file> <path> [sha256]   fetch a page from the Syphon server through the tunnel
#[path = "../src/vpn.rs"]
#[allow(dead_code)]
mod vpn;
use sha2::Digest;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let c = vpn::decode_code(&a[2])?;
    match a[1].as_str() {
        "enroll" => {
            let own = vpn::new_private();
            std::fs::write(&a[3], &own)?;
            vpn::enroll(&c, &own, &|s: &str| eprintln!("{s}")).await?;
            println!("enrolled");
        }
        "welcome-get" | "get" => {
            let key = if a[1] == "get" { std::fs::read_to_string(&a[3])? } else { c.private.clone() };
            let node = vpn::node_addr(&c.node)?;
            let t = vpn::Tunnel::start(&c.endpoint, &c.server_public, &c.psk, key.trim(), vpn::address_of(&c.address)?, vec![*node.ip()]).await?;
            if !t.wait_up(std::time::Duration::from_secs(8)).await {
                anyhow::bail!("no handshake");
            }
            let port = t.forward(node).await?;
            let started = std::time::Instant::now();
            let r = reqwest::Client::builder().no_proxy().build()?.get(format!("http://127.0.0.1:{port}{}", a[4])).timeout(std::time::Duration::from_secs(60)).send().await?;
            let b = r.bytes().await?;
            let secs = started.elapsed().as_secs_f64();
            if b.len() > 100_000 {
                println!("{} {} {:.0}", b.len(), hex::encode(sha2::Sha256::digest(&b)), b.len() as f64 * 8.0 / secs / 1e6);
            } else {
                println!("{}", String::from_utf8_lossy(&b));
            }
        }
        _ => anyhow::bail!("?"),
    }
    Ok(())
}
