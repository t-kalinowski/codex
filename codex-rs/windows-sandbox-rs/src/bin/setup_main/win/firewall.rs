use anyhow::Result;
use std::io::Write;

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Foundation::VARIANT_FALSE;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwPolicy2;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwRule3;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_ACTION_BLOCK;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_AUTHENTICATE_NONE;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_EDGE_TRAVERSAL_TYPE_DENY;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_ANY;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_TCP;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_UDP;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_MODIFY_STATE;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_MODIFY_STATE_OK;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE_TYPE2;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE2_ALL;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE2_DOMAIN;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE2_PRIVATE;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE2_PUBLIC;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_RULE_DIR_OUT;
use windows::Win32::NetworkManagement::WindowsFirewall::NetFwPolicy2;
use windows::Win32::NetworkManagement::WindowsFirewall::NetFwRule;
use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::Com::CoCreateInstance;
use windows::Win32::System::Com::CoInitializeEx;
use windows::Win32::System::Com::CoUninitialize;
use windows::core::BSTR;
use windows::core::HRESULT;
use windows::core::Interface;
use windows::core::VARIANT;

use codex_windows_sandbox::SetupErrorCode;
use codex_windows_sandbox::SetupFailure;
use codex_windows_sandbox::WindowsSandboxPolicyNamespace;

// This is the stable identifier we use to find/update the rule idempotently.
// It intentionally does not change between installs.
const OFFLINE_BLOCK_RULE_NAME: &str = "codex_sandbox_offline_block_outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME: &str = "codex_sandbox_offline_block_loopback_tcp";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME: &str = "codex_sandbox_offline_block_loopback_udp";
const MCP_CONSOLE_OFFLINE_BLOCK_RULE_NAME: &str = "mcp_console_sandbox_offline_block_outbound";
const MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME: &str =
    "mcp_console_sandbox_offline_block_loopback_tcp";
const MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME: &str =
    "mcp_console_sandbox_offline_block_loopback_udp";

// Friendly text shown in the firewall UI.
const OFFLINE_BLOCK_RULE_FRIENDLY: &str = "Codex Sandbox Offline - Block Non-Loopback Outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY: &str =
    "Codex Sandbox Offline - Block Loopback TCP (Except Proxy)";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY: &str = "Codex Sandbox Offline - Block Loopback UDP";
const OFFLINE_PROXY_ALLOW_RULE_NAME: &str = "codex_sandbox_offline_allow_loopback_proxy";
const MCP_CONSOLE_OFFLINE_BLOCK_RULE_FRIENDLY: &str =
    "MCP Console Sandbox Offline - Block Non-Loopback Outbound";
const MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY: &str =
    "MCP Console Sandbox Offline - Block Loopback TCP (Except Proxy)";
const MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY: &str =
    "MCP Console Sandbox Offline - Block Loopback UDP";
const MCP_CONSOLE_OFFLINE_PROXY_ALLOW_RULE_NAME: &str =
    "mcp_console_sandbox_offline_allow_loopback_proxy";
const LOOPBACK_REMOTE_ADDRESSES: &str = "127.0.0.0/8,::/127";
const NON_LOOPBACK_REMOTE_ADDRESSES: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";
const ALL_NETWORK_VALUES: &str = "*";
const ALL_INTERFACE_TYPES: &str = "All";
const EMPTY_RULE_VALUE: &str = "";

#[derive(Clone, Copy)]
struct FirewallRuleNames {
    outbound_block: &'static str,
    loopback_tcp_block: &'static str,
    loopback_udp_block: &'static str,
    proxy_allow: &'static str,
    outbound_block_friendly: &'static str,
    loopback_tcp_block_friendly: &'static str,
    loopback_udp_block_friendly: &'static str,
    exact_scope: bool,
}

impl FirewallRuleNames {
    #[cfg(test)]
    fn all(self) -> [&'static str; 4] {
        [
            self.outbound_block,
            self.loopback_tcp_block,
            self.loopback_udp_block,
            self.proxy_allow,
        ]
    }
}

fn firewall_rule_names(namespace: WindowsSandboxPolicyNamespace) -> FirewallRuleNames {
    match namespace {
        WindowsSandboxPolicyNamespace::Codex => FirewallRuleNames {
            outbound_block: OFFLINE_BLOCK_RULE_NAME,
            loopback_tcp_block: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
            loopback_udp_block: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
            proxy_allow: OFFLINE_PROXY_ALLOW_RULE_NAME,
            outbound_block_friendly: OFFLINE_BLOCK_RULE_FRIENDLY,
            loopback_tcp_block_friendly: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
            loopback_udp_block_friendly: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY,
            exact_scope: false,
        },
        WindowsSandboxPolicyNamespace::McpConsole => FirewallRuleNames {
            outbound_block: MCP_CONSOLE_OFFLINE_BLOCK_RULE_NAME,
            loopback_tcp_block: MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
            loopback_udp_block: MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
            proxy_allow: MCP_CONSOLE_OFFLINE_PROXY_ALLOW_RULE_NAME,
            outbound_block_friendly: MCP_CONSOLE_OFFLINE_BLOCK_RULE_FRIENDLY,
            loopback_tcp_block_friendly: MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
            loopback_udp_block_friendly: MCP_CONSOLE_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY,
            exact_scope: true,
        },
    }
}

struct BlockRuleSpec<'a> {
    internal_name: &'a str,
    friendly_desc: &'a str,
    protocol: i32,
    local_user_spec: &'a str,
    offline_sid: &'a str,
    remote_addresses: Option<&'a str>,
    remote_ports: Option<&'a str>,
    exact_scope: bool,
}

fn outbound_block_spec<'a>(
    rule_names: &FirewallRuleNames,
    local_user_spec: &'a str,
    offline_sid: &'a str,
) -> BlockRuleSpec<'a> {
    BlockRuleSpec {
        internal_name: rule_names.outbound_block,
        friendly_desc: rule_names.outbound_block_friendly,
        protocol: NET_FW_IP_PROTOCOL_ANY.0,
        local_user_spec,
        offline_sid,
        remote_addresses: Some(NON_LOOPBACK_REMOTE_ADDRESSES),
        remote_ports: None,
        exact_scope: rule_names.exact_scope,
    }
}

fn loopback_udp_block_spec<'a>(
    rule_names: &FirewallRuleNames,
    local_user_spec: &'a str,
    offline_sid: &'a str,
) -> BlockRuleSpec<'a> {
    BlockRuleSpec {
        internal_name: rule_names.loopback_udp_block,
        friendly_desc: rule_names.loopback_udp_block_friendly,
        protocol: NET_FW_IP_PROTOCOL_UDP.0,
        local_user_spec,
        offline_sid,
        remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
        remote_ports: None,
        exact_scope: rule_names.exact_scope,
    }
}

fn loopback_tcp_block_spec<'a>(
    rule_names: &FirewallRuleNames,
    local_user_spec: &'a str,
    offline_sid: &'a str,
    remote_ports: Option<&'a str>,
) -> BlockRuleSpec<'a> {
    BlockRuleSpec {
        internal_name: rule_names.loopback_tcp_block,
        friendly_desc: rule_names.loopback_tcp_block_friendly,
        protocol: NET_FW_IP_PROTOCOL_TCP.0,
        local_user_spec,
        offline_sid,
        remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
        remote_ports,
        exact_scope: rule_names.exact_scope,
    }
}

pub fn ensure_offline_proxy_allowlist(
    namespace: WindowsSandboxPolicyNamespace,
    offline_sid: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
    log: &mut dyn Write,
) -> Result<()> {
    let rule_names = firewall_rule_names(namespace);
    let local_user_spec = format!("O:LSD:(A;;CC;;;{offline_sid})");

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallComInitFailed,
            format!("CoInitializeEx failed: {hr:?}"),
        )));
    }

    let result = unsafe {
        (|| -> Result<()> {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| {
                    anyhow::Error::new(SetupFailure::new(
                        SetupErrorCode::HelperFirewallPolicyAccessFailed,
                        format!("CoCreateInstance NetFwPolicy2 failed: {err:?}"),
                    ))
                })?;
            ensure_local_policy_rules_take_effect(&policy, namespace)?;
            let rules = policy.Rules().map_err(|err| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperFirewallPolicyAccessFailed,
                    format!("INetFwPolicy2::Rules failed: {err:?}"),
                ))
            })?;

            if allow_local_binding {
                // Remove the legacy overlapping allow rule before returning to the local-binding
                // mode so stale proxy exceptions do not linger.
                remove_rule_if_present(&rules, rule_names.proxy_allow, log)?;
                remove_rule_if_present(&rules, rule_names.loopback_udp_block, log)?;
                remove_rule_if_present(&rules, rule_names.loopback_tcp_block, log)?;
                return Ok(());
            }

            ensure_block_rule(
                &rules,
                &loopback_udp_block_spec(&rule_names, &local_user_spec, offline_sid),
                log,
            )?;

            // Install a broad TCP loopback block before narrowing it to the allowed proxy port
            // complement. If the narrowing update fails, the sandbox remains fail-closed.
            ensure_block_rule(
                &rules,
                &loopback_tcp_block_spec(&rule_names, &local_user_spec, offline_sid, None),
                log,
            )?;

            // Remove the legacy overlapping allow rule only after the explicit block rules are in
            // place so transitions back to proxy-only mode do not fail open.
            remove_rule_if_present(&rules, rule_names.proxy_allow, log)?;

            if let Some(blocked_remote_ports) = blocked_loopback_tcp_remote_ports(proxy_ports) {
                ensure_block_rule(
                    &rules,
                    &loopback_tcp_block_spec(
                        &rule_names,
                        &local_user_spec,
                        offline_sid,
                        Some(&blocked_remote_ports),
                    ),
                    log,
                )?;
            }
            Ok(())
        })()
    };

    unsafe {
        CoUninitialize();
    }
    result
}

pub fn ensure_offline_outbound_block(
    namespace: WindowsSandboxPolicyNamespace,
    offline_sid: &str,
    log: &mut dyn Write,
) -> Result<()> {
    let rule_names = firewall_rule_names(namespace);
    let local_user_spec = format!("O:LSD:(A;;CC;;;{offline_sid})");

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallComInitFailed,
            format!("CoInitializeEx failed: {hr:?}"),
        )));
    }

    let result = unsafe {
        (|| -> Result<()> {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| {
                    anyhow::Error::new(SetupFailure::new(
                        SetupErrorCode::HelperFirewallPolicyAccessFailed,
                        format!("CoCreateInstance NetFwPolicy2 failed: {err:?}"),
                    ))
                })?;
            ensure_local_policy_rules_take_effect(&policy, namespace)?;
            let rules = policy.Rules().map_err(|err| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperFirewallPolicyAccessFailed,
                    format!("INetFwPolicy2::Rules failed: {err:?}"),
                ))
            })?;

            // Block all outbound IP protocols for this user.
            ensure_block_rule(
                &rules,
                &outbound_block_spec(&rule_names, &local_user_spec, offline_sid),
                log,
            )?;
            Ok(())
        })()
    };

    unsafe {
        CoUninitialize();
    }
    result
}

pub fn verify_offline_sandbox_network(
    namespace: WindowsSandboxPolicyNamespace,
    offline_sid: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<()> {
    let rule_names = firewall_rule_names(namespace);
    let local_user_spec = format!("O:LSD:(A;;CC;;;{offline_sid})");
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallComInitFailed,
            format!("CoInitializeEx failed: {hr:?}"),
        )));
    }

    let result = unsafe {
        (|| -> Result<()> {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| {
                    anyhow::Error::new(SetupFailure::new(
                        SetupErrorCode::HelperFirewallPolicyAccessFailed,
                        format!("CoCreateInstance NetFwPolicy2 failed: {err:?}"),
                    ))
                })?;
            ensure_local_policy_rules_take_effect(&policy, namespace)?;
            let rules = policy.Rules().map_err(|err| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperFirewallPolicyAccessFailed,
                    format!("INetFwPolicy2::Rules failed: {err:?}"),
                ))
            })?;

            verify_block_rule(
                &rules,
                &outbound_block_spec(&rule_names, &local_user_spec, offline_sid),
            )?;
            verify_rule_absent(&rules, rule_names.proxy_allow)?;
            if allow_local_binding {
                verify_rule_absent(&rules, rule_names.loopback_udp_block)?;
                verify_rule_absent(&rules, rule_names.loopback_tcp_block)?;
            } else {
                verify_block_rule(
                    &rules,
                    &loopback_udp_block_spec(&rule_names, &local_user_spec, offline_sid),
                )?;
                let blocked_remote_ports = blocked_loopback_tcp_remote_ports(proxy_ports);
                verify_block_rule(
                    &rules,
                    &loopback_tcp_block_spec(
                        &rule_names,
                        &local_user_spec,
                        offline_sid,
                        blocked_remote_ports.as_deref(),
                    ),
                )?;
            }
            Ok(())
        })()
    };

    unsafe {
        CoUninitialize();
    }
    result
}

fn remove_rule_if_present(
    rules: &INetFwRules,
    internal_name: &str,
    log: &mut dyn Write,
) -> Result<()> {
    let name = BSTR::from(internal_name);
    if unsafe { rules.Item(&name) }.is_ok() {
        unsafe { rules.Remove(&name) }.map_err(|err| {
            anyhow::Error::new(SetupFailure::new(
                SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
                format!("Rules::Remove failed for {internal_name}: {err:?}"),
            ))
        })?;
        log_line(log, &format!("firewall rule removed name={internal_name}"))?;
    }
    Ok(())
}

fn ensure_local_policy_rules_take_effect(
    policy: &INetFwPolicy2,
    namespace: WindowsSandboxPolicyNamespace,
) -> Result<()> {
    let mut modify_state = NET_FW_MODIFY_STATE::default();
    let result = unsafe {
        (Interface::vtable(policy).LocalPolicyModifyState)(
            Interface::as_raw(policy),
            &mut modify_state,
        )
    };
    validate_local_policy_modify_result(result, modify_state)?;
    if namespace == WindowsSandboxPolicyNamespace::McpConsole {
        verify_active_profile_firewalls_enabled(policy)?;
    }
    Ok(())
}

fn verify_active_profile_firewalls_enabled(policy: &INetFwPolicy2) -> Result<()> {
    let active_profiles = unsafe { policy.CurrentProfileTypes() }.map_err(|error| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallPolicyAccessFailed,
            format!("INetFwPolicy2::CurrentProfileTypes failed: {error:?}"),
        ))
    })?;
    let known_profiles =
        NET_FW_PROFILE2_DOMAIN.0 | NET_FW_PROFILE2_PRIVATE.0 | NET_FW_PROFILE2_PUBLIC.0;
    let unknown_profiles = active_profiles & !known_profiles;
    if unknown_profiles != 0 {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallPolicyIneffective,
            format!(
                "active Windows Firewall profile mask contains unsupported bits: active={active_profiles:#x}, unknown={unknown_profiles:#x}"
            ),
        )));
    }

    for (profile, name) in [
        (NET_FW_PROFILE2_DOMAIN, "domain"),
        (NET_FW_PROFILE2_PRIVATE, "private"),
        (NET_FW_PROFILE2_PUBLIC, "public"),
    ] {
        if active_profiles & profile.0 == 0 {
            continue;
        }
        let enabled = unsafe { policy.get_FirewallEnabled(profile) }.map_err(|error| {
            anyhow::Error::new(SetupFailure::new(
                SetupErrorCode::HelperFirewallPolicyAccessFailed,
                format!("INetFwPolicy2::FirewallEnabled failed for {name} profile: {error:?}"),
            ))
        })?;
        validate_active_profile_firewall_state(profile, name, enabled)?;
    }
    Ok(())
}

fn validate_active_profile_firewall_state(
    profile: NET_FW_PROFILE_TYPE2,
    name: &str,
    enabled: windows::Win32::Foundation::VARIANT_BOOL,
) -> Result<()> {
    if enabled == VARIANT_TRUE {
        return Ok(());
    }
    Err(anyhow::Error::new(SetupFailure::new(
        SetupErrorCode::HelperFirewallPolicyIneffective,
        format!(
            "Windows Firewall is disabled for the active {name} profile ({:#x}); sandbox firewall rules cannot enforce network policy",
            profile.0
        ),
    )))
}

fn validate_local_policy_modify_result(
    result: windows::core::HRESULT,
    modify_state: NET_FW_MODIFY_STATE,
) -> Result<()> {
    if result.is_err() {
        // The COM query itself failed, so Windows never gave us a policy answer.
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallPolicyAccessFailed,
            format!("INetFwPolicy2::LocalPolicyModifyState failed: {result:?}"),
        )));
    }

    if result != S_OK {
        // S_FALSE means the answer only holds for some active profiles, not all of them.
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallPolicyIneffective,
            format!(
                "local firewall policy modifications do not apply to every current profile: LocalPolicyModifyState result={result:?}"
            ),
        )));
    }

    if modify_state == NET_FW_MODIFY_STATE_OK {
        return Ok(());
    }

    // Windows answered uniformly, and that answer says local rule edits are ineffective.
    Err(anyhow::Error::new(SetupFailure::new(
        SetupErrorCode::HelperFirewallPolicyIneffective,
        format!(
            "local firewall policy modifications will not take effect: LocalPolicyModifyState={modify_state:?}"
        ),
    )))
}

fn ensure_block_rule(
    rules: &INetFwRules,
    spec: &BlockRuleSpec<'_>,
    log: &mut dyn Write,
) -> Result<()> {
    let name = BSTR::from(spec.internal_name);
    let rule: INetFwRule3 = match unsafe { rules.Item(&name) } {
        Ok(existing) => existing.cast().map_err(|err| {
            anyhow::Error::new(SetupFailure::new(
                SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
                format!("cast existing firewall rule to INetFwRule3 failed: {err:?}"),
            ))
        })?,
        Err(_) => {
            let new_rule: INetFwRule3 =
                unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }.map_err(
                    |err| {
                        anyhow::Error::new(SetupFailure::new(
                            SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
                            format!("CoCreateInstance NetFwRule failed: {err:?}"),
                        ))
                    },
                )?;
            unsafe { new_rule.SetName(&name) }.map_err(|err| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
                    format!("SetName failed: {err:?}"),
                ))
            })?;
            // Set all properties before adding the rule so we don't leave half-configured rules.
            configure_rule(&new_rule, spec)?;
            unsafe { rules.Add(&new_rule) }.map_err(|err| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
                    format!("Rules::Add failed: {err:?}"),
                ))
            })?;
            new_rule
        }
    };

    // Always re-apply fields to keep the setup idempotent.
    configure_rule(&rule, spec)?;

    let remote_addresses_log = spec.remote_addresses.unwrap_or("*");
    let remote_ports_log = spec.remote_ports.unwrap_or("*");

    log_line(
        log,
        &format!(
            "firewall rule configured name={} protocol={} RemoteAddresses={remote_addresses_log} RemotePorts={remote_ports_log} LocalUserAuthorizedList={}",
            spec.internal_name, spec.protocol, spec.local_user_spec
        ),
    )?;
    Ok(())
}

fn verify_rule_absent(rules: &INetFwRules, internal_name: &str) -> Result<()> {
    let name = BSTR::from(internal_name);
    match unsafe { rules.Item(&name) } {
        Ok(_) => Err(firewall_verification_error(format!(
            "unexpected stale firewall rule is present: {internal_name}"
        ))),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => Ok(()),
        Err(error) => Err(firewall_verification_error(format!(
            "inspect absent firewall rule {internal_name}: {error:?}"
        ))),
    }
}

fn verify_block_rule(rules: &INetFwRules, spec: &BlockRuleSpec<'_>) -> Result<()> {
    let name = BSTR::from(spec.internal_name);
    let rule: INetFwRule3 = unsafe { rules.Item(&name) }
        .map_err(|error| {
            firewall_verification_error(format!(
                "required firewall rule {} is unavailable: {error:?}",
                spec.internal_name
            ))
        })?
        .cast()
        .map_err(|error| {
            firewall_verification_error(format!(
                "cast firewall rule {} to INetFwRule3: {error:?}",
                spec.internal_name
            ))
        })?;
    verify_configured_rule(&rule, spec)
}

fn verify_configured_rule(rule: &INetFwRule3, spec: &BlockRuleSpec<'_>) -> Result<()> {
    let actual_description = unsafe { rule.Description() }
        .map_err(|error| rule_read_error(spec, "Description", error))?
        .to_string();
    let actual_direction =
        unsafe { rule.Direction() }.map_err(|error| rule_read_error(spec, "Direction", error))?;
    let actual_action =
        unsafe { rule.Action() }.map_err(|error| rule_read_error(spec, "Action", error))?;
    let actual_enabled =
        unsafe { rule.Enabled() }.map_err(|error| rule_read_error(spec, "Enabled", error))?;
    let actual_profiles =
        unsafe { rule.Profiles() }.map_err(|error| rule_read_error(spec, "Profiles", error))?;
    let actual_protocol =
        unsafe { rule.Protocol() }.map_err(|error| rule_read_error(spec, "Protocol", error))?;
    let actual_remote_addresses = unsafe { rule.RemoteAddresses() }
        .map_err(|error| rule_read_error(spec, "RemoteAddresses", error))?
        .to_string();
    let actual_remote_ports = if actual_protocol == NET_FW_IP_PROTOCOL_TCP.0
        || actual_protocol == NET_FW_IP_PROTOCOL_UDP.0
    {
        unsafe { rule.RemotePorts() }
            .map_err(|error| rule_read_error(spec, "RemotePorts", error))?
            .to_string()
    } else {
        "*".to_string()
    };
    let actual_user_scope = unsafe { rule.LocalUserAuthorizedList() }
        .map_err(|error| rule_read_error(spec, "LocalUserAuthorizedList", error))?
        .to_string();
    let expected_remote_addresses = spec.remote_addresses.unwrap_or(ALL_NETWORK_VALUES);
    let expected_remote_ports = spec.remote_ports.unwrap_or(ALL_NETWORK_VALUES);
    let mut mismatches = Vec::new();
    record_mismatch(
        &mut mismatches,
        "description",
        spec.friendly_desc,
        &actual_description,
    );
    record_mismatch(
        &mut mismatches,
        "direction",
        &NET_FW_RULE_DIR_OUT,
        &actual_direction,
    );
    record_mismatch(
        &mut mismatches,
        "action",
        &NET_FW_ACTION_BLOCK,
        &actual_action,
    );
    record_mismatch(&mut mismatches, "enabled", &VARIANT_TRUE, &actual_enabled);
    record_mismatch(
        &mut mismatches,
        "profiles",
        &NET_FW_PROFILE2_ALL.0,
        &actual_profiles,
    );
    record_mismatch(
        &mut mismatches,
        "protocol",
        &spec.protocol,
        &actual_protocol,
    );
    let expected_remote_addresses = canonical_rule_scope(
        spec,
        CanonicalRuleScope::RemoteAddresses,
        expected_remote_addresses,
    )?;
    let expected_remote_ports =
        if spec.protocol == NET_FW_IP_PROTOCOL_TCP.0 || spec.protocol == NET_FW_IP_PROTOCOL_UDP.0 {
            canonical_rule_scope(
                spec,
                CanonicalRuleScope::RemotePorts(spec.protocol),
                expected_remote_ports,
            )?
        } else {
            ALL_NETWORK_VALUES.to_string()
        };
    record_mismatch(
        &mut mismatches,
        "remote_addresses",
        &expected_remote_addresses,
        &actual_remote_addresses,
    );
    record_mismatch(
        &mut mismatches,
        "remote_ports",
        &expected_remote_ports,
        &actual_remote_ports,
    );
    record_mismatch(
        &mut mismatches,
        "local_user_authorized_list",
        spec.local_user_spec,
        &actual_user_scope,
    );

    if spec.exact_scope {
        verify_exact_rule_scope(rule, spec, &mut mismatches)?;
    }

    if !mismatches.is_empty() {
        return Err(firewall_verification_error(format!(
            "firewall rule {} differs from the requested policy: {}",
            spec.internal_name,
            mismatches.join(", ")
        )));
    }
    Ok(())
}

fn verify_exact_rule_scope(
    rule: &INetFwRule3,
    spec: &BlockRuleSpec<'_>,
    mismatches: &mut Vec<String>,
) -> Result<()> {
    let name = read_rule_string(unsafe { rule.Name() }, spec, "Name")?;
    let application_name =
        read_rule_string(unsafe { rule.ApplicationName() }, spec, "ApplicationName")?;
    let service_name = read_rule_string(unsafe { rule.ServiceName() }, spec, "ServiceName")?;
    let local_addresses =
        read_rule_string(unsafe { rule.LocalAddresses() }, spec, "LocalAddresses")?;
    let local_ports =
        if spec.protocol == NET_FW_IP_PROTOCOL_TCP.0 || spec.protocol == NET_FW_IP_PROTOCOL_UDP.0 {
            read_rule_string(unsafe { rule.LocalPorts() }, spec, "LocalPorts")?
        } else {
            ALL_NETWORK_VALUES.to_string()
        };
    let interfaces =
        unsafe { rule.Interfaces() }.map_err(|error| rule_read_error(spec, "Interfaces", error))?;
    let interface_types =
        read_rule_string(unsafe { rule.InterfaceTypes() }, spec, "InterfaceTypes")?;
    let edge_traversal = unsafe { rule.EdgeTraversal() }
        .map_err(|error| rule_read_error(spec, "EdgeTraversal", error))?;
    let edge_traversal_options = unsafe { rule.EdgeTraversalOptions() }
        .map_err(|error| rule_read_error(spec, "EdgeTraversalOptions", error))?;
    let local_app_package_id = read_rule_string(
        unsafe { rule.LocalAppPackageId() },
        spec,
        "LocalAppPackageId",
    )?;
    let local_user_owner =
        read_rule_string(unsafe { rule.LocalUserOwner() }, spec, "LocalUserOwner")?;
    let remote_user_authorized_list = read_rule_string(
        unsafe { rule.RemoteUserAuthorizedList() },
        spec,
        "RemoteUserAuthorizedList",
    )?;
    let remote_machine_authorized_list = read_rule_string(
        unsafe { rule.RemoteMachineAuthorizedList() },
        spec,
        "RemoteMachineAuthorizedList",
    )?;
    let secure_flags = unsafe { rule.SecureFlags() }
        .map_err(|error| rule_read_error(spec, "SecureFlags", error))?;

    record_mismatch(mismatches, "name", spec.internal_name, &name);
    record_mismatch(
        mismatches,
        "application_name",
        EMPTY_RULE_VALUE,
        &application_name,
    );
    record_mismatch(mismatches, "service_name", EMPTY_RULE_VALUE, &service_name);
    let expected_local_addresses =
        canonical_rule_scope(spec, CanonicalRuleScope::LocalAddresses, ALL_NETWORK_VALUES)?;
    let expected_local_ports =
        if spec.protocol == NET_FW_IP_PROTOCOL_TCP.0 || spec.protocol == NET_FW_IP_PROTOCOL_UDP.0 {
            canonical_rule_scope(
                spec,
                CanonicalRuleScope::LocalPorts(spec.protocol),
                ALL_NETWORK_VALUES,
            )?
        } else {
            ALL_NETWORK_VALUES.to_string()
        };
    record_mismatch(
        mismatches,
        "local_addresses",
        &expected_local_addresses,
        &local_addresses,
    );
    record_mismatch(
        mismatches,
        "local_ports",
        &expected_local_ports,
        &local_ports,
    );
    if !interfaces.is_empty() {
        mismatches.push(format!(
            "interfaces expected=VARIANT_EMPTY actual={interfaces:?}"
        ));
    }
    let expected_interface_types = canonical_rule_scope(
        spec,
        CanonicalRuleScope::InterfaceTypes,
        ALL_INTERFACE_TYPES,
    )?;
    record_mismatch(
        mismatches,
        "interface_types",
        &expected_interface_types,
        &interface_types,
    );
    record_mismatch(
        mismatches,
        "edge_traversal",
        &VARIANT_FALSE,
        &edge_traversal,
    );
    record_mismatch(
        mismatches,
        "edge_traversal_options",
        &NET_FW_EDGE_TRAVERSAL_TYPE_DENY.0,
        &edge_traversal_options,
    );
    record_mismatch(
        mismatches,
        "local_app_package_id",
        EMPTY_RULE_VALUE,
        &local_app_package_id,
    );
    record_mismatch(
        mismatches,
        "local_user_owner",
        EMPTY_RULE_VALUE,
        &local_user_owner,
    );
    record_mismatch(
        mismatches,
        "remote_user_authorized_list",
        EMPTY_RULE_VALUE,
        &remote_user_authorized_list,
    );
    record_mismatch(
        mismatches,
        "remote_machine_authorized_list",
        EMPTY_RULE_VALUE,
        &remote_machine_authorized_list,
    );
    record_mismatch(
        mismatches,
        "secure_flags",
        &NET_FW_AUTHENTICATE_NONE.0,
        &secure_flags,
    );
    Ok(())
}

fn read_rule_string(
    result: windows::core::Result<BSTR>,
    spec: &BlockRuleSpec<'_>,
    property: &str,
) -> Result<String> {
    Ok(result
        .map_err(|error| rule_read_error(spec, property, error))?
        .to_string())
}

fn record_mismatch<T: std::fmt::Debug + PartialEq + ?Sized>(
    mismatches: &mut Vec<String>,
    property: &str,
    expected: &T,
    actual: &T,
) {
    if actual != expected {
        mismatches.push(format!(
            "{property} expected={expected:?} actual={actual:?}"
        ));
    }
}

fn rule_read_error(
    spec: &BlockRuleSpec<'_>,
    property: &str,
    error: windows::core::Error,
) -> anyhow::Error {
    firewall_verification_error(format!(
        "read firewall rule {} property {property}: {error:?}",
        spec.internal_name
    ))
}

fn firewall_verification_error(message: String) -> anyhow::Error {
    anyhow::Error::new(SetupFailure::new(
        SetupErrorCode::HelperFirewallRuleVerifyFailed,
        message,
    ))
}

fn configure_rule(rule: &INetFwRule3, spec: &BlockRuleSpec<'_>) -> Result<()> {
    unsafe {
        rule.SetDescription(&BSTR::from(spec.friendly_desc))
            .map_err(|error| rule_configuration_error("Description", error))?;
        rule.SetDirection(NET_FW_RULE_DIR_OUT)
            .map_err(|error| rule_configuration_error("Direction", error))?;
        rule.SetAction(NET_FW_ACTION_BLOCK)
            .map_err(|error| rule_configuration_error("Action", error))?;
        rule.SetEnabled(VARIANT_TRUE)
            .map_err(|error| rule_configuration_error("Enabled", error))?;
        rule.SetProfiles(NET_FW_PROFILE2_ALL.0)
            .map_err(|error| rule_configuration_error("Profiles", error))?;
        configure_rule_network_scope(rule, spec)?;
        if spec.exact_scope {
            configure_exact_rule_scope(rule)?;
        }
        rule.SetLocalUserAuthorizedList(&BSTR::from(spec.local_user_spec))
            .map_err(|error| rule_configuration_error("LocalUserAuthorizedList", error))?;
    }

    // Read-back verification: ensure we actually wrote the expected SID scope.
    let actual = unsafe { rule.LocalUserAuthorizedList() }.map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallRuleVerifyFailed,
            format!("LocalUserAuthorizedList (read-back) failed: {err:?}"),
        ))
    })?;
    let actual_str = actual.to_string();
    if !actual_str.contains(spec.offline_sid) {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallRuleVerifyFailed,
            format!(
                "offline firewall rule user scope mismatch: expected SID {}, got {actual_str}",
                spec.offline_sid
            ),
        )));
    }
    Ok(())
}

fn configure_rule_network_scope(rule: &INetFwRule3, spec: &BlockRuleSpec<'_>) -> Result<()> {
    unsafe {
        rule.SetProtocol(spec.protocol)
            .map_err(|error| rule_configuration_error("Protocol", error))?;
        let remote_addresses = spec.remote_addresses.unwrap_or(ALL_NETWORK_VALUES);
        rule.SetRemoteAddresses(&BSTR::from(remote_addresses))
            .map_err(|error| rule_configuration_error("RemoteAddresses", error))?;
        if spec.protocol == NET_FW_IP_PROTOCOL_TCP.0 || spec.protocol == NET_FW_IP_PROTOCOL_UDP.0 {
            let remote_ports = spec.remote_ports.unwrap_or(ALL_NETWORK_VALUES);
            rule.SetRemotePorts(&BSTR::from(remote_ports))
                .map_err(|error| rule_configuration_error("RemotePorts", error))?;
        }
    }

    Ok(())
}

fn configure_exact_rule_scope(rule: &INetFwRule3) -> Result<()> {
    let empty = BSTR::from(EMPTY_RULE_VALUE);
    unsafe {
        rule.SetApplicationName(&empty)
            .map_err(|error| rule_configuration_error("ApplicationName", error))?;
        rule.SetServiceName(&empty)
            .map_err(|error| rule_configuration_error("ServiceName", error))?;
        rule.SetLocalAddresses(&BSTR::from(ALL_NETWORK_VALUES))
            .map_err(|error| rule_configuration_error("LocalAddresses", error))?;
        let protocol = rule
            .Protocol()
            .map_err(|error| rule_configuration_error("Protocol read-back", error))?;
        if protocol == NET_FW_IP_PROTOCOL_TCP.0 || protocol == NET_FW_IP_PROTOCOL_UDP.0 {
            rule.SetLocalPorts(&BSTR::from(ALL_NETWORK_VALUES))
                .map_err(|error| rule_configuration_error("LocalPorts", error))?;
        }
        rule.SetInterfaces(&VARIANT::new())
            .map_err(|error| rule_configuration_error("Interfaces", error))?;
        rule.SetInterfaceTypes(&BSTR::from(ALL_INTERFACE_TYPES))
            .map_err(|error| rule_configuration_error("InterfaceTypes", error))?;
        rule.SetEdgeTraversal(VARIANT_FALSE)
            .map_err(|error| rule_configuration_error("EdgeTraversal", error))?;
        rule.SetEdgeTraversalOptions(NET_FW_EDGE_TRAVERSAL_TYPE_DENY.0)
            .map_err(|error| rule_configuration_error("EdgeTraversalOptions", error))?;
        rule.SetLocalAppPackageId(&empty)
            .map_err(|error| rule_configuration_error("LocalAppPackageId", error))?;
        rule.SetLocalUserOwner(&empty)
            .map_err(|error| rule_configuration_error("LocalUserOwner", error))?;
        rule.SetRemoteUserAuthorizedList(&empty)
            .map_err(|error| rule_configuration_error("RemoteUserAuthorizedList", error))?;
        rule.SetRemoteMachineAuthorizedList(&empty)
            .map_err(|error| rule_configuration_error("RemoteMachineAuthorizedList", error))?;
        rule.SetSecureFlags(NET_FW_AUTHENTICATE_NONE.0)
            .map_err(|error| rule_configuration_error("SecureFlags", error))?;
    }
    Ok(())
}

fn rule_configuration_error(property: &str, error: windows::core::Error) -> anyhow::Error {
    anyhow::Error::new(SetupFailure::new(
        SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
        format!("set firewall rule property {property}: {error:?}"),
    ))
}

enum CanonicalRuleScope {
    RemoteAddresses,
    RemotePorts(i32),
    LocalAddresses,
    LocalPorts(i32),
    InterfaceTypes,
}

fn canonical_rule_scope(
    spec: &BlockRuleSpec<'_>,
    property: CanonicalRuleScope,
    value: &str,
) -> Result<String> {
    let rule: INetFwRule3 = unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }
        .map_err(|error| {
            firewall_verification_error(format!(
                "create temporary firewall rule to canonicalize {} policy: {error:?}",
                spec.internal_name
            ))
        })?;
    let value = BSTR::from(value);
    let result = unsafe {
        (|| -> windows::core::Result<BSTR> {
            match property {
                CanonicalRuleScope::RemoteAddresses => {
                    rule.SetRemoteAddresses(&value)?;
                    rule.RemoteAddresses()
                }
                CanonicalRuleScope::RemotePorts(protocol) => {
                    rule.SetProtocol(protocol)?;
                    rule.SetRemotePorts(&value)?;
                    rule.RemotePorts()
                }
                CanonicalRuleScope::LocalAddresses => {
                    rule.SetLocalAddresses(&value)?;
                    rule.LocalAddresses()
                }
                CanonicalRuleScope::LocalPorts(protocol) => {
                    rule.SetProtocol(protocol)?;
                    rule.SetLocalPorts(&value)?;
                    rule.LocalPorts()
                }
                CanonicalRuleScope::InterfaceTypes => {
                    rule.SetInterfaceTypes(&value)?;
                    rule.InterfaceTypes()
                }
            }
        })()
    };
    Ok(result
        .map_err(|error| {
            firewall_verification_error(format!(
                "canonicalize firewall rule {} scope: {error:?}",
                spec.internal_name
            ))
        })?
        .to_string())
}

fn blocked_loopback_tcp_remote_ports(proxy_ports: &[u16]) -> Option<String> {
    let mut allowed_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    allowed_ports.sort_unstable();
    allowed_ports.dedup();
    if allowed_ports.is_empty() {
        return None;
    }

    let mut blocked_ranges = Vec::new();
    let mut start = 1_u32;
    for port in allowed_ports {
        let port = u32::from(port);
        if port < start {
            continue;
        }
        if port > start {
            blocked_ranges.push(port_range_string(start, port - 1));
        }
        start = port + 1;
    }

    if start <= u32::from(u16::MAX) {
        blocked_ranges.push(port_range_string(start, u32::from(u16::MAX)));
    }

    if blocked_ranges.is_empty() {
        None
    } else {
        Some(blocked_ranges.join(","))
    }
}

fn port_range_string(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn log_line(log: &mut dyn Write, msg: &str) -> Result<()> {
    let ts = chrono::Utc::now().to_rfc3339();
    writeln!(log, "[{ts}] {msg}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use codex_windows_sandbox::WindowsSandboxPolicyNamespace;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;
    use windows::Win32::Foundation::S_FALSE;
    use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_MODIFY_STATE_GP_OVERRIDE;

    use super::*;

    #[test]
    fn policy_namespace_rule_names_are_disjoint() {
        let codex = firewall_rule_names(WindowsSandboxPolicyNamespace::Codex)
            .all()
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mcp_console = firewall_rule_names(WindowsSandboxPolicyNamespace::McpConsole)
            .all()
            .into_iter()
            .collect::<BTreeSet<_>>();

        assert!(codex.is_disjoint(&mcp_console));
    }

    #[test]
    fn empty_proxy_port_set_uses_the_broad_tcp_block() {
        assert_eq!(blocked_loopback_tcp_remote_ports(&[]), None);
    }

    #[test]
    fn configured_remote_address_literals_are_accepted_by_firewall_com() {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert!(hr.is_ok(), "CoInitializeEx failed: {hr:?}");

        let candidates = [
            LOOPBACK_REMOTE_ADDRESSES,
            NON_LOOPBACK_REMOTE_ADDRESSES,
            "*",
        ];
        let results = candidates.map(|remote_addresses| unsafe {
            let rule: windows::core::Result<INetFwRule3> =
                CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER);
            rule.and_then(|rule| {
                rule.SetRemoteAddresses(&BSTR::from(remote_addresses))?;
                rule.RemoteAddresses()
            })
            .map(|stored| stored.to_string())
        });

        unsafe {
            CoUninitialize();
        }

        for (remote_addresses, result) in candidates.into_iter().zip(results) {
            assert!(
                result.is_ok(),
                "firewall rejected RemoteAddresses={remote_addresses:?}: {result:?}"
            );
        }
    }

    #[test]
    fn exact_firewall_rule_scope_round_trips_through_com_and_detects_drift() {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert!(hr.is_ok(), "CoInitializeEx failed: {hr:?}");

        let result = unsafe {
            (|| -> Result<()> {
                let rule: INetFwRule3 = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
                let spec = BlockRuleSpec {
                    internal_name: MCP_CONSOLE_OFFLINE_BLOCK_RULE_NAME,
                    friendly_desc: MCP_CONSOLE_OFFLINE_BLOCK_RULE_FRIENDLY,
                    protocol: NET_FW_IP_PROTOCOL_ANY.0,
                    local_user_spec: "O:LSD:(A;;CC;;;S-1-5-18)",
                    offline_sid: "S-1-5-18",
                    remote_addresses: Some(NON_LOOPBACK_REMOTE_ADDRESSES),
                    remote_ports: None,
                    exact_scope: true,
                };
                rule.SetName(&BSTR::from(spec.internal_name))?;
                configure_rule(&rule, &spec)?;
                verify_configured_rule(&rule, &spec)?;

                rule.SetRemoteAddresses(&BSTR::from(ALL_NETWORK_VALUES))?;
                let error = verify_configured_rule(&rule, &spec)
                    .expect_err("remote-address widening must be detected");
                let failure = error
                    .downcast_ref::<SetupFailure>()
                    .expect("expected setup failure");
                assert_eq!(failure.code, SetupErrorCode::HelperFirewallRuleVerifyFailed);
                assert!(failure.message.contains("remote_addresses"));

                configure_rule(&rule, &spec)?;
                rule.SetInterfaceTypes(&BSTR::from("Wireless"))?;
                let error = verify_configured_rule(&rule, &spec)
                    .expect_err("interface narrowing must be detected");
                let failure = error
                    .downcast_ref::<SetupFailure>()
                    .expect("expected setup failure");
                assert_eq!(failure.code, SetupErrorCode::HelperFirewallRuleVerifyFailed);
                assert!(failure.message.contains("interface_types"));
                Ok(())
            })()
        };

        unsafe {
            CoUninitialize();
        }
        assert!(
            result.is_ok(),
            "exact firewall rule COM round-trip: {result:?}"
        );
    }

    #[test]
    fn production_firewall_rule_network_scopes_are_accepted_by_firewall_com() {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert!(hr.is_ok(), "CoInitializeEx failed: {hr:?}");

        let local_user_spec = "O:LSD:(A;;CC;;;S-1-5-18)";
        let offline_sid = "S-1-5-18";
        let blocked_remote_ports =
            blocked_loopback_tcp_remote_ports(&[8080]).expect("proxy-port complement should exist");
        let broad_tcp_remote_ports = blocked_loopback_tcp_remote_ports(&[]);
        let specs = [
            BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_UDP.0,
                local_user_spec,
                offline_sid,
                remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: None,
                exact_scope: true,
            },
            BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_TCP.0,
                local_user_spec,
                offline_sid,
                remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: Some(&blocked_remote_ports),
                exact_scope: true,
            },
            BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_TCP.0,
                local_user_spec,
                offline_sid,
                remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: broad_tcp_remote_ports.as_deref(),
                exact_scope: true,
            },
            BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_ANY.0,
                local_user_spec,
                offline_sid,
                remote_addresses: Some(NON_LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: None,
                exact_scope: true,
            },
        ];

        let results = specs.each_ref().map(|spec| unsafe {
            let rule: windows::core::Result<INetFwRule3> =
                CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER);
            match rule {
                Ok(rule) => configure_rule_network_scope(&rule, spec),
                Err(err) => Err(err.into()),
            }
        });

        unsafe {
            CoUninitialize();
        }

        for (spec, result) in specs.into_iter().zip(results) {
            assert!(
                result.is_ok(),
                "firewall rejected network scope for rule={} protocol={} remote_addresses={:?} remote_ports={:?}: {result:?}",
                spec.internal_name,
                spec.protocol,
                spec.remote_addresses,
                spec.remote_ports
            );
        }
    }

    #[test]
    fn local_policy_modify_state_accepts_effective_policy() {
        assert!(validate_local_policy_modify_result(S_OK, NET_FW_MODIFY_STATE_OK).is_ok());
    }

    #[test]
    fn local_policy_modify_state_rejects_ineffective_policy() {
        let err = validate_local_policy_modify_result(S_OK, NET_FW_MODIFY_STATE_GP_OVERRIDE)
            .expect_err("group-policy override should fail sandbox firewall setup");
        let failure = err
            .downcast_ref::<SetupFailure>()
            .expect("expected setup failure");

        assert_eq!(
            failure.code,
            SetupErrorCode::HelperFirewallPolicyIneffective
        );
    }

    #[test]
    fn local_policy_modify_state_rejects_partial_profile_coverage() {
        let err = validate_local_policy_modify_result(S_FALSE, NET_FW_MODIFY_STATE_OK)
            .expect_err("partial profile coverage should fail sandbox firewall setup");
        let failure = err
            .downcast_ref::<SetupFailure>()
            .expect("expected setup failure");

        assert_eq!(
            failure.code,
            SetupErrorCode::HelperFirewallPolicyIneffective
        );
    }

    #[test]
    fn disabled_active_firewall_profile_is_rejected() {
        let error =
            validate_active_profile_firewall_state(NET_FW_PROFILE2_PUBLIC, "public", VARIANT_FALSE)
                .expect_err("disabled active profile should fail sandbox firewall setup");
        let failure = error
            .downcast_ref::<SetupFailure>()
            .expect("expected setup failure");

        assert_eq!(
            failure.code,
            SetupErrorCode::HelperFirewallPolicyIneffective
        );
        assert!(failure.message.contains("active public profile"));
    }
}
