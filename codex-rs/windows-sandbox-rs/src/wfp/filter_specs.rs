use crate::policy_namespace::WindowsSandboxPolicyNamespace;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMP;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMPV6;
use windows_sys::core::GUID;

#[derive(Clone, Copy)]
pub(super) enum ConditionSpec {
    User,
    Protocol(u8),
    RemotePort(u16),
}

#[derive(Clone, Copy)]
pub(super) struct FilterSpec {
    codex_key: GUID,
    mcp_console_key: GUID,
    codex_name: &'static str,
    mcp_console_name: &'static str,
    pub(super) description: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
}

impl FilterSpec {
    pub(super) const fn key(self, namespace: WindowsSandboxPolicyNamespace) -> GUID {
        match namespace {
            WindowsSandboxPolicyNamespace::Codex => self.codex_key,
            WindowsSandboxPolicyNamespace::McpConsole => self.mcp_console_key,
        }
    }

    pub(super) const fn name(self, namespace: WindowsSandboxPolicyNamespace) -> &'static str {
        match namespace {
            WindowsSandboxPolicyNamespace::Codex => self.codex_name,
            WindowsSandboxPolicyNamespace::McpConsole => self.mcp_console_name,
        }
    }
}

pub(super) const FILTER_SPECS: &[FilterSpec] = &[
    FilterSpec {
        codex_key: GUID::from_u128(0x9f5f3812_79f0_4fe9_9615_4c2c92d2f0ff),
        mcp_console_key: GUID::from_u128(0x51b90ce5_2e26_47be_bd22_975898b4e09c),
        codex_name: "codex_wfp_icmp_connect_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_icmp_connect_v4",
        description: "Block sandbox-account ICMP connect v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0x87498484_45ab_4510_845e_ece8b791b3bc),
        mcp_console_key: GUID::from_u128(0xbd016c89_57f2_480c_b37a_bea0d8224e73),
        codex_name: "codex_wfp_icmp_connect_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_icmp_connect_v6",
        description: "Block sandbox-account ICMP connect v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xaf4751de_f874_4a7b_a34d_f0d0f22d1d9b),
        mcp_console_key: GUID::from_u128(0x342a445f_fabf_4f3a_b955_a398df839839),
        codex_name: "codex_wfp_icmp_assign_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_icmp_assign_v4",
        description: "Block sandbox-account ICMP resource assignment v4",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xea10db66_a928_4b2e_a82e_a376a54f93ba),
        mcp_console_key: GUID::from_u128(0x5747b0df_a08e_4bcb_9f70_54e860632582),
        codex_name: "codex_wfp_icmp_assign_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_icmp_assign_v6",
        description: "Block sandbox-account ICMP resource assignment v6",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    // NAME_RESOLUTION_CACHE filters are intentionally omitted because ordinary
    // static filter shapes returned FWP_E_OUT_OF_BOUNDS during validation.
    FilterSpec {
        codex_key: GUID::from_u128(0x83172805_f6be_4ae1_9dc6_6847aef04e7f),
        mcp_console_key: GUID::from_u128(0x9ff5e9f4_8c84_4ad4_94c8_a146ab13ed6b),
        codex_name: "codex_wfp_dns_53_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_dns_53_v4",
        description: "Block sandbox-account DNS TCP or UDP port 53 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xd23b2efb_1efb_46b2_96f3_b0ccda5690c8),
        mcp_console_key: GUID::from_u128(0x88984699_d880_4c4d_ae5a_095c5cbc7225),
        codex_name: "codex_wfp_dns_53_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_dns_53_v6",
        description: "Block sandbox-account DNS TCP or UDP port 53 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0x420b026f_9dc9_4aea_88f4_0f2b9feab39a),
        mcp_console_key: GUID::from_u128(0x6fe46228_4afa_4d65_afa6_7c1a3b5ae9de),
        codex_name: "codex_wfp_dns_853_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_dns_853_v4",
        description: "Block sandbox-account DNS-over-TLS port 853 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0x8d917c81_99cc_45e7_84d6_824df860cfb8),
        mcp_console_key: GUID::from_u128(0x6f10f4bd_5640_4f50_b9cf_ebfde4b17ab8),
        codex_name: "codex_wfp_dns_853_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_dns_853_v6",
        description: "Block sandbox-account DNS-over-TLS port 853 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xe1d6e0af_ce5f_471b_b2d3_15ca00e966f3),
        mcp_console_key: GUID::from_u128(0x9a14edac_30e9_404a_b52e_9b48aded2442),
        codex_name: "codex_wfp_smb_445_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_smb_445_v4",
        description: "Block sandbox-account SMB port 445 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xc2bceca4_66ef_4a0f_ba80_f4f761b8c6f0),
        mcp_console_key: GUID::from_u128(0x9d01857b_b900_4700_91e1_afd5faec1fd2),
        codex_name: "codex_wfp_smb_445_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_smb_445_v6",
        description: "Block sandbox-account SMB port 445 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xba10c618_84e7_4b83_8f74_36e22b2fa1ff),
        mcp_console_key: GUID::from_u128(0xba59dc5f_049d_4e56_80d7_a73c9af0775e),
        codex_name: "codex_wfp_smb_139_v4",
        mcp_console_name: "mcp_console_sandbox_wfp_smb_139_v4",
        description: "Block sandbox-account SMB port 139 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
    FilterSpec {
        codex_key: GUID::from_u128(0xfe7f22b8_5cf5_4adb_b2aa_71fc0a8f5d44),
        mcp_console_key: GUID::from_u128(0x08a3b4e2_d4e5_42c1_997d_2ae19d8ca7c7),
        codex_name: "codex_wfp_smb_139_v6",
        mcp_console_name: "mcp_console_sandbox_wfp_smb_139_v6",
        description: "Block sandbox-account SMB port 139 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
];
