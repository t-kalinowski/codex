use crate::environment::TargetEnvironment;
use crate::protocol::LoopbackPolicy;
use crate::protocol::ManagedNetworkAccess;
use crate::protocol::NetworkPolicy;
use crate::protocol::UnixSocketAccess;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_network_proxy::NetworkDomainPermission;
use codex_network_proxy::NetworkDomainPermissionEntry;
use codex_network_proxy::NetworkDomainPermissions;
use codex_network_proxy::NetworkMode;
use codex_network_proxy::NetworkProxy;
use codex_network_proxy::NetworkProxyConfig;
use codex_network_proxy::NetworkProxyHandle;
use codex_network_proxy::NetworkProxyState;
use codex_network_proxy::NetworkUnixSocketPermission;
use codex_network_proxy::NetworkUnixSocketPermissions;
use codex_network_proxy::RemoteNetworkProxyConfig;
use codex_network_proxy::RemoteNetworkProxyLaunchConfig;
use codex_protocol::permissions::NetworkSandboxPolicy;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

pub struct PreparedNetwork {
    pub sandbox_policy: NetworkSandboxPolicy,
    pub enforce_managed_network: bool,
    pub proxy: Option<NetworkProxy>,
    pub sandbox_context: Option<ManagedNetworkSandboxContext>,
    pub handle: Option<NetworkProxyHandle>,
}

pub fn validate_network_policy(policy: &NetworkPolicy) -> Result<()> {
    let NetworkPolicy::ManagedProxy {
        allowed_domains: _,
        denied_domains: _,
        socks_udp,
        local_binding,
        loopback,
        local_ports,
        unix_sockets,
        ..
    } = policy
    else {
        return Ok(());
    };
    if *socks_udp {
        bail!("managed SOCKS UDP is unsupported by the native confinement backends")
    }
    if !local_ports.is_empty() {
        bail!("explicit managed-network local port exceptions are unsupported")
    }
    let loopback_allowed = matches!(loopback, LoopbackPolicy::Allow);
    if loopback_allowed != *local_binding {
        bail!(
            "this release requires loopback=allow and local_binding=true, or loopback=proxy_only and local_binding=false"
        )
    }
    #[cfg(target_os = "linux")]
    if !local_binding {
        bail!("Linux managed networking cannot enforce local_binding=false in this release")
    }
    if !cfg!(target_os = "macos") && !unix_sockets.is_empty() {
        bail!("managed Unix-socket policy is supported only on macOS")
    }
    let mut unix_socket_paths = BTreeSet::new();
    for rule in unix_sockets {
        if rule.access == UnixSocketAccess::Deny {
            bail!(
                "managed Unix-socket deny rules are unsupported; protocol version 1 is allowlist-only"
            )
        }
        if !Path::new(&rule.path).is_absolute() {
            bail!("managed Unix-socket policy path must be absolute")
        }
        if !unix_socket_paths.insert(&rule.path) {
            bail!("managed Unix-socket policy contains a duplicate path")
        }
    }
    Ok(())
}

pub async fn prepare_network(
    policy: &NetworkPolicy,
    environment: &mut TargetEnvironment,
) -> Result<PreparedNetwork> {
    validate_network_policy(policy)?;
    match policy {
        NetworkPolicy::Denied => Ok(PreparedNetwork {
            sandbox_policy: NetworkSandboxPolicy::Restricted,
            enforce_managed_network: false,
            proxy: None,
            sandbox_context: None,
            handle: None,
        }),
        NetworkPolicy::Unrestricted => Ok(PreparedNetwork {
            sandbox_policy: NetworkSandboxPolicy::Enabled,
            enforce_managed_network: false,
            proxy: None,
            sandbox_context: None,
            handle: None,
        }),
        NetworkPolicy::ManagedProxy {
            access,
            allowed_domains,
            denied_domains,
            socks,
            upstream_proxy,
            local_binding,
            unix_sockets,
            ..
        } => {
            let unix_sockets = unix_sockets
                .iter()
                .map(|rule| {
                    let path = Path::new(&rule.path);
                    debug_assert!(path.is_absolute());
                    let permission = match rule.access {
                        UnixSocketAccess::Allow => NetworkUnixSocketPermission::Allow,
                        UnixSocketAccess::Deny => NetworkUnixSocketPermission::Deny,
                    };
                    Ok((rule.path.clone(), permission))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let domains = allowed_domains
                .iter()
                .map(|pattern| NetworkDomainPermissionEntry {
                    pattern: pattern.clone(),
                    permission: NetworkDomainPermission::Allow,
                })
                .chain(
                    denied_domains
                        .iter()
                        .map(|pattern| NetworkDomainPermissionEntry {
                            pattern: pattern.clone(),
                            permission: NetworkDomainPermission::Deny,
                        }),
                )
                .collect::<Vec<_>>();
            let config = NetworkProxyConfig {
                enabled: true,
                enable_socks5: *socks,
                enable_socks5_udp: false,
                allow_upstream_proxy: *upstream_proxy,
                dangerously_allow_non_loopback_proxy: false,
                dangerously_allow_all_unix_sockets: false,
                mode: match access {
                    ManagedNetworkAccess::Full => NetworkMode::Full,
                    ManagedNetworkAccess::Limited => NetworkMode::Limited,
                },
                domains: (!domains.is_empty())
                    .then_some(NetworkDomainPermissions { entries: domains }),
                unix_sockets: (!unix_sockets.is_empty()).then_some(NetworkUnixSocketPermissions {
                    entries: unix_sockets,
                }),
                allow_local_binding: *local_binding,
                mitm: false,
                credential_broker: false,
                credential_broker_openai_host: None,
                dangerously_allow_plaintext_credential_injection: false,
                mitm_hooks: Vec::new(),
                ..NetworkProxyConfig::default()
            };
            let remote = RemoteNetworkProxyConfig::from_effective_config(&config)
                .context("managed network policy is invalid")?;
            let state = NetworkProxyState::from_remote_launch_config(
                RemoteNetworkProxyLaunchConfig::new(remote),
            )
            .context("managed network state could not be created")?;
            let proxy = NetworkProxy::builder()
                .state(Arc::new(state))
                .build()
                .await
                .context("managed network proxy could not be prepared")?;
            let handle = proxy
                .run()
                .await
                .context("managed network proxy could not start")?;
            let prepared = match proxy.prepare_for_optional_environment(
                std::mem::take(environment),
                /*environment_id*/ None,
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = handle.shutdown().await;
                    return Err(error)
                        .context("managed network target environment could not be prepared");
                }
            };
            *environment = prepared.env;
            Ok(PreparedNetwork {
                sandbox_policy: NetworkSandboxPolicy::Restricted,
                enforce_managed_network: true,
                proxy: Some(proxy),
                sandbox_context: Some(prepared.sandbox_context),
                handle: Some(handle),
            })
        }
    }
}
