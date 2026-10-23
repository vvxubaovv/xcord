use std::time::Duration;

use xcord_ssh::{AuthMethod, SshOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = async_channel::bounded(128);
    let options = SshOptions {
        host: "test.rebex.net".into(),
        port: 22,
        user: "demo".into(),
        auth: AuthMethod::Password("password".into()),
        cols: 100,
        rows: 30,
        term: "xterm-256color".into(),
        command: None,
        connect_timeout: Duration::from_secs(20),
        ..Default::default()
    };
    let transport = xcord_ssh::connect(options, tx).await?;
    println!(
        "connected: {} key={} {} known={}",
        transport.server(),
        transport.host_key().algorithm,
        transport.host_key().fingerprint,
        transport.host_key().known
    );

    let mut output = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Ok(xcord_ssh::TransportEvent::Output(data))) => {
                output.push_str(&String::from_utf8_lossy(&data));
                if !output.is_empty() {
                    // Keep reading briefly to collect command output.
                    if output.lines().count() > 6 {
                        break;
                    }
                }
            }
            Ok(Ok(xcord_ssh::TransportEvent::Connected { .. })) => {}
            Ok(Ok(xcord_ssh::TransportEvent::Error(message))) => {
                eprintln!("transport error: {message}")
            }
            Ok(Ok(xcord_ssh::TransportEvent::Closed { reason, .. })) => {
                println!("closed: {reason}");
                break;
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    println!("---- output ----\n{output}");
    transport.close();
    Ok(())
}
