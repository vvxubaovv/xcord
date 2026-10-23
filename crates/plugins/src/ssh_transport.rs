use std::rc::Rc;
use std::time::Duration;

use xcord_cordis::{Context, Error, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, AuthRequest, ConnectSpec, ConnectionInfo, HostKeySummary, Runtime, RuntimeHandle,
    Transport, TransportEvent, TransportProvider, Transports,
};

/// Registers the SSH transport.
#[derive(Clone)]
pub struct SshTransportPlugin;

impl Plugin for SshTransportPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("ssh", "SSH Transport")
            .description("russh-based SSH-2 client with TOFU host key verification")
            .author("xcord")
            .capabilities(&["transport", "ssh"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::RUNTIME, services::TRANSPORTS]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?;
        let transports = ctx.require::<Transports>(services::TRANSPORTS)?;

        let provider: Rc<dyn TransportProvider> = Rc::new(SshProvider {
            runtime: runtime.handle(),
        });
        transports
            .register(provider)
            .map_err(|message| Error::PluginFailed {
                name: "ssh",
                message,
            })?;

        ctx.effect("unregister ssh transport", move || {
            transports.unregister("ssh");
        });
        Ok(())
    }
}

struct SshProvider {
    runtime: RuntimeHandle,
}

impl TransportProvider for SshProvider {
    fn kind(&self) -> &'static str {
        "ssh"
    }

    fn connect(
        &self,
        spec: ConnectSpec,
        events: async_channel::Sender<TransportEvent>,
    ) -> std::result::Result<(), String> {
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            let (ssh_events_tx, ssh_events_rx) = async_channel::bounded(1024);
            let options = to_ssh_options(&spec);
            match xcord_ssh::connect(options, ssh_events_tx).await {
                Ok(inner) => {
                    let info = ConnectionInfo {
                        server: inner.server().to_string(),
                        kind: "ssh".into(),
                        host_key: HostKeySummary {
                            algorithm: inner.host_key().algorithm.clone(),
                            fingerprint: inner.host_key().fingerprint.clone(),
                            known: inner.host_key().known,
                            accepted: inner.host_key().accepted,
                        },
                    };
                    let transport: Box<dyn Transport> = Box::new(SshAdapter { inner });
                    if events
                        .send(TransportEvent::Connected { transport, info })
                        .await
                        .is_err()
                    {
                        return;
                    }

                    while let Ok(event) = ssh_events_rx.recv().await {
                        let event = match event {
                            xcord_ssh::TransportEvent::Connected { .. } => continue,
                            xcord_ssh::TransportEvent::Output(data) => TransportEvent::Output(data),
                            xcord_ssh::TransportEvent::Error(message) => {
                                TransportEvent::Error(message)
                            }
                            xcord_ssh::TransportEvent::Closed {
                                reason,
                                exit_status,
                            } => TransportEvent::Closed {
                                reason,
                                exit_status,
                            },
                        };
                        if events.send(event).await.is_err() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    log::warn!("ssh: connection failed: {error}");
                    let _ = events
                        .send(TransportEvent::Error(format!("SSH: {error}")))
                        .await;
                    let _ = events
                        .send(TransportEvent::Closed {
                            reason: "connection failed".into(),
                            exit_status: None,
                        })
                        .await;
                }
            }
        });
        Ok(())
    }
}

fn to_ssh_options(spec: &ConnectSpec) -> xcord_ssh::SshOptions {
    let auth = match &spec.auth {
        AuthRequest::Password(password) => xcord_ssh::AuthMethod::Password(password.clone()),
        AuthRequest::PublicKey { path, passphrase } => xcord_ssh::AuthMethod::PublicKey {
            path: path.clone(),
            passphrase: passphrase.clone(),
        },
        AuthRequest::None => xcord_ssh::AuthMethod::None,
    };
    xcord_ssh::SshOptions {
        host: spec.host.clone(),
        port: spec.port,
        user: spec.user.clone(),
        auth,
        cols: spec.cols,
        rows: spec.rows,
        term: spec.term.clone(),
        command: spec.command.clone(),
        connect_timeout: spec.connect_timeout.max(Duration::from_millis(500)),
        keepalive: spec.keepalive,
        host_key_policy: if spec.accept_new_host_keys {
            xcord_ssh::HostKeyPolicy::AcceptNew
        } else {
            xcord_ssh::HostKeyPolicy::Strict
        },
        known_hosts: spec.known_hosts.clone(),
    }
}

struct SshAdapter {
    inner: xcord_ssh::SshTransport,
}

impl Transport for SshAdapter {
    fn write(&self, data: &[u8]) -> std::result::Result<(), String> {
        self.inner.write(data).map_err(|error| error.to_string())
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.inner.resize(cols, rows);
    }

    fn close(&self) {
        self.inner.close();
    }

    fn description(&self) -> String {
        self.inner.server().to_string()
    }
}
