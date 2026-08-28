mod filter_specs;

use crate::WindowsSandboxPolicyNamespace;
use crate::to_wide;
use crate::winutil::resolve_sid;
use anyhow::Result;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::mem::zeroed;
use std::ptr::null;
use std::ptr::null_mut;
use std::slice;
use windows_sys::Win32::Foundation::FWP_E_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::FWP_E_FILTER_NOT_FOUND;
use windows_sys::Win32::Foundation::FWP_E_NOT_FOUND;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTRL_MATCH_FILTER;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_BYTE_BLOB;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_EMPTY;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_EQUAL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_SECURITY_DESCRIPTOR_TYPE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT8;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT16;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_ALE_USER_ID;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_PROTOCOL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_REMOTE_PORT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_DISPLAY_DATA0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_CONDITION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SESSION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineClose0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineOpen0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterDeleteByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterGetByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFreeMemory0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmProviderAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmProviderGetByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmSubLayerAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmSubLayerGetByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionAbort0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionBegin0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionCommit0;
use windows_sys::Win32::Security::Authorization::BuildSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::core::GUID;

use filter_specs::ConditionSpec;
use filter_specs::FILTER_SPECS;
use filter_specs::FilterSpec;

const SESSION_NAME: &str = "Codex Windows Sandbox WFP";
const PROVIDER_NAME: &str = "Codex Windows Sandbox WFP";
const PROVIDER_DESCRIPTION: &str = "Persistent WFP provider for Codex Windows sandbox filters";
const SUBLAYER_NAME: &str = "Codex Windows Sandbox WFP";
const SUBLAYER_DESCRIPTION: &str = "Persistent WFP sublayer for Codex Windows sandbox filters";
const SUBLAYER_WEIGHT: u16 = 0x8000;

// WFP identifies persistent providers, sublayers, and filters by stable GUIDs.
// These values are Codex-owned identities; do not regenerate them unless we
// intentionally want to orphan old objects and create a new WFP namespace.
const PROVIDER_KEY: GUID = GUID::from_u128(0x2e31d31c_3948_4753_9117_e5d1a6496f41);
const SUBLAYER_KEY: GUID = GUID::from_u128(0xe65054fd_4d32_4c7c_95ef_621f0cf6431a);

/// Installs the persistent Codex WFP filters for `account`.
///
/// This is intended to run from the already-elevated setup helper. Callers
/// should treat any returned error as non-fatal to the rest of setup.
pub fn install_wfp_filters_for_account(account: &str) -> Result<usize> {
    install_wfp_filters_for_account_in_namespace(account, WindowsSandboxPolicyNamespace::Codex)
}

/// Installs the persistent WFP filters for one closed Windows sandbox policy namespace.
#[doc(hidden)]
pub fn install_wfp_filters_for_account_in_namespace(
    account: &str,
    namespace: WindowsSandboxPolicyNamespace,
) -> Result<usize> {
    let engine = Engine::open()?;
    let mut transaction = engine.begin_transaction()?;
    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    let user_condition = UserMatchCondition::for_account(account)?;
    let mut installed_filter_count = 0;
    for spec in FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key(namespace))?;
        add_filter(engine.handle, spec, namespace, &user_condition)?;
        installed_filter_count += 1;
    }

    transaction.commit()?;
    Ok(installed_filter_count)
}

/// Verifies every authority-bearing field of the persistent WFP filters for
/// one closed Windows sandbox policy namespace.
#[doc(hidden)]
pub fn verify_wfp_filters_for_account_in_namespace(
    account: &str,
    namespace: WindowsSandboxPolicyNamespace,
) -> Result<()> {
    let engine = Engine::open()?;
    verify_provider(engine.handle)?;
    verify_sublayer(engine.handle)?;
    let user_condition = UserMatchCondition::for_account(account)?;
    for spec in FILTER_SPECS {
        verify_filter(engine.handle, spec, namespace, &user_condition)?;
    }
    Ok(())
}

struct RetrievedFilter(*mut FWPM_FILTER0);

impl RetrievedFilter {
    fn get(engine: HANDLE, key: &GUID) -> Result<Self> {
        let mut filter = null_mut();
        let result = unsafe { FwpmFilterGetByKey0(engine, key, &mut filter) };
        ensure_success(result, "FwpmFilterGetByKey0")?;
        if filter.is_null() {
            anyhow::bail!("FwpmFilterGetByKey0 returned a null filter");
        }
        Ok(Self(filter))
    }

    fn as_ref(&self) -> &FWPM_FILTER0 {
        unsafe { &*self.0 }
    }
}

impl Drop for RetrievedFilter {
    fn drop(&mut self) {
        let mut filter = self.0.cast::<c_void>();
        unsafe { FwpmFreeMemory0(&mut filter) };
    }
}

struct RetrievedProvider(*mut FWPM_PROVIDER0);

impl RetrievedProvider {
    fn get(engine: HANDLE) -> Result<Self> {
        let mut provider = null_mut();
        let result = unsafe { FwpmProviderGetByKey0(engine, &PROVIDER_KEY, &mut provider) };
        ensure_success(result, "FwpmProviderGetByKey0")?;
        if provider.is_null() {
            anyhow::bail!("FwpmProviderGetByKey0 returned a null provider");
        }
        Ok(Self(provider))
    }

    fn as_ref(&self) -> &FWPM_PROVIDER0 {
        unsafe { &*self.0 }
    }
}

impl Drop for RetrievedProvider {
    fn drop(&mut self) {
        let mut provider = self.0.cast::<c_void>();
        unsafe { FwpmFreeMemory0(&mut provider) };
    }
}

struct RetrievedSublayer(*mut FWPM_SUBLAYER0);

impl RetrievedSublayer {
    fn get(engine: HANDLE) -> Result<Self> {
        let mut sublayer = null_mut();
        let result = unsafe { FwpmSubLayerGetByKey0(engine, &SUBLAYER_KEY, &mut sublayer) };
        ensure_success(result, "FwpmSubLayerGetByKey0")?;
        if sublayer.is_null() {
            anyhow::bail!("FwpmSubLayerGetByKey0 returned a null sublayer");
        }
        Ok(Self(sublayer))
    }

    fn as_ref(&self) -> &FWPM_SUBLAYER0 {
        unsafe { &*self.0 }
    }
}

impl Drop for RetrievedSublayer {
    fn drop(&mut self) {
        let mut sublayer = self.0.cast::<c_void>();
        unsafe { FwpmFreeMemory0(&mut sublayer) };
    }
}

fn verify_provider(engine: HANDLE) -> Result<()> {
    let retrieved = RetrievedProvider::get(engine)
        .map_err(|error| anyhow::anyhow!("WFP provider is unavailable: {error}"))?;
    let provider = retrieved.as_ref();
    if !guid_eq(&provider.providerKey, &PROVIDER_KEY) {
        anyhow::bail!("WFP provider has an incompatible key");
    }
    if provider.flags != FWPM_PROVIDER_FLAG_PERSISTENT {
        anyhow::bail!(
            "WFP provider has incompatible flags 0x{:08X}; expected persistent-only flags 0x{:08X}",
            provider.flags,
            FWPM_PROVIDER_FLAG_PERSISTENT
        );
    }
    if provider.providerData.size != 0 {
        anyhow::bail!("WFP provider has incompatible provider data");
    }
    if !provider.serviceName.is_null() {
        anyhow::bail!("WFP provider is unexpectedly associated with a Windows service");
    }
    Ok(())
}

fn verify_sublayer(engine: HANDLE) -> Result<()> {
    let retrieved = RetrievedSublayer::get(engine)
        .map_err(|error| anyhow::anyhow!("WFP sublayer is unavailable: {error}"))?;
    let sublayer = retrieved.as_ref();
    if !guid_eq(&sublayer.subLayerKey, &SUBLAYER_KEY) {
        anyhow::bail!("WFP sublayer has an incompatible key");
    }
    if sublayer.flags != FWPM_SUBLAYER_FLAG_PERSISTENT {
        anyhow::bail!(
            "WFP sublayer has incompatible flags 0x{:08X}; expected persistent-only flags 0x{:08X}",
            sublayer.flags,
            FWPM_SUBLAYER_FLAG_PERSISTENT
        );
    }
    if sublayer.providerKey.is_null()
        || !unsafe { guid_eq(&*sublayer.providerKey, &PROVIDER_KEY) }
    {
        anyhow::bail!("WFP sublayer is associated with an incompatible provider");
    }
    if sublayer.providerData.size != 0 {
        anyhow::bail!("WFP sublayer has incompatible provider data");
    }
    if sublayer.weight != SUBLAYER_WEIGHT {
        anyhow::bail!(
            "WFP sublayer has incompatible weight 0x{:04X}; expected 0x{SUBLAYER_WEIGHT:04X}",
            sublayer.weight
        );
    }
    Ok(())
}

fn verify_filter(
    engine: HANDLE,
    spec: &FilterSpec,
    namespace: WindowsSandboxPolicyNamespace,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let expected_key = spec.key(namespace);
    let retrieved = RetrievedFilter::get(engine, &expected_key).map_err(|error| {
        anyhow::anyhow!(
            "WFP filter {} is unavailable: {error}",
            spec.name(namespace)
        )
    })?;
    let filter = retrieved.as_ref();
    let provider_matches =
        !filter.providerKey.is_null() && unsafe { guid_eq(&*filter.providerKey, &PROVIDER_KEY) };
    if !guid_eq(&filter.filterKey, &expected_key)
        || !provider_matches
        || !guid_eq(&filter.layerKey, &spec.layer_key)
        || !guid_eq(&filter.subLayerKey, &SUBLAYER_KEY)
        || filter.flags != FWPM_FILTER_FLAG_PERSISTENT
        || filter.action.r#type != FWP_ACTION_BLOCK
        || filter.weight.r#type != FWP_EMPTY
    {
        anyhow::bail!(
            "WFP filter {} has incompatible authority fields",
            spec.name(namespace)
        );
    }
    verify_filter_conditions(filter, spec, user_condition).map_err(|error| {
        anyhow::anyhow!(
            "WFP filter {} is incompatible: {error}",
            spec.name(namespace)
        )
    })
}

fn verify_filter_conditions(
    filter: &FWPM_FILTER0,
    spec: &FilterSpec,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let actual_count = usize::try_from(filter.numFilterConditions)?;
    if actual_count != spec.conditions.len() {
        anyhow::bail!(
            "expected {} conditions, found {actual_count}",
            spec.conditions.len()
        );
    }
    if actual_count != 0 && filter.filterCondition.is_null() {
        anyhow::bail!("filter condition array is null");
    }
    let actual_conditions = unsafe { slice::from_raw_parts(filter.filterCondition, actual_count) };
    for (actual, expected) in actual_conditions.iter().zip(spec.conditions) {
        if actual.matchType != FWP_MATCH_EQUAL {
            anyhow::bail!("filter condition does not use exact matching");
        }
        match expected {
            ConditionSpec::User => {
                if !guid_eq(&actual.fieldKey, &FWPM_CONDITION_ALE_USER_ID)
                    || actual.conditionValue.r#type != FWP_SECURITY_DESCRIPTOR_TYPE
                {
                    anyhow::bail!("filter user condition has an incompatible shape");
                }
                let actual_blob = unsafe { actual.conditionValue.Anonymous.sd };
                let actual_bytes = unsafe { security_descriptor_bytes(actual_blob)? };
                let expected_bytes = unsafe { security_descriptor_bytes(&user_condition.blob)? };
                if actual_bytes != expected_bytes {
                    anyhow::bail!("filter user condition targets a different identity");
                }
            }
            ConditionSpec::Protocol(protocol) => {
                if !guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_PROTOCOL)
                    || actual.conditionValue.r#type != FWP_UINT8
                    || unsafe { actual.conditionValue.Anonymous.uint8 } != *protocol
                {
                    anyhow::bail!("filter protocol condition differs from the requested policy");
                }
            }
            ConditionSpec::RemotePort(port) => {
                if !guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_REMOTE_PORT)
                    || actual.conditionValue.r#type != FWP_UINT16
                    || unsafe { actual.conditionValue.Anonymous.uint16 } != *port
                {
                    anyhow::bail!("filter port condition differs from the requested policy");
                }
            }
        }
    }
    Ok(())
}

fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

unsafe fn security_descriptor_bytes<'a>(blob: *const FWP_BYTE_BLOB) -> Result<&'a [u8]> {
    if blob.is_null() {
        anyhow::bail!("filter security descriptor is null");
    }
    let blob = unsafe { &*blob };
    if blob.size == 0 {
        return Ok(&[]);
    }
    if blob.data.is_null() {
        anyhow::bail!("filter security descriptor data is null");
    }
    Ok(unsafe { slice::from_raw_parts(blob.data, blob.size as usize) })
}

/// Owns an open WFP engine handle and closes it on drop.
struct Engine {
    handle: HANDLE,
}

impl Engine {
    fn open() -> Result<Self> {
        let session_name = to_wide(OsStr::new(SESSION_NAME));
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: session_name.as_ptr() as *mut _,
            description: null_mut(),
        };
        session.txnWaitTimeoutInMSec = INFINITE;

        let mut handle = HANDLE::default();
        let result = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        };
        ensure_success(result, "FwpmEngineOpen0")?;
        Ok(Self { handle })
    }

    fn begin_transaction(&self) -> Result<Transaction<'_>> {
        let result = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        ensure_success(result, "FwpmTransactionBegin0")?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

/// Aborts an open WFP transaction unless it was explicitly committed.
struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(&mut self) -> Result<()> {
        let result = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        ensure_success(result, "FwpmTransactionCommit0")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

/// Builds the ALE_USER_ID condition blob that scopes filters to one account.
struct UserMatchCondition {
    security_descriptor: PSECURITY_DESCRIPTOR,
    blob: FWP_BYTE_BLOB,
}

impl UserMatchCondition {
    fn for_account(account: &str) -> Result<Self> {
        let sid = resolve_sid(account)?;
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FWP_ACTRL_MATCH_FILTER,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.as_ptr() as *mut u16,
            },
        };

        let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let mut security_descriptor_len = 0;
        let result = unsafe {
            BuildSecurityDescriptorW(
                null(),
                null(),
                1,
                &access,
                0,
                null(),
                null_mut(),
                &mut security_descriptor_len,
                &mut security_descriptor,
            )
        };
        ensure_success(result, "BuildSecurityDescriptorW")?;

        Ok(Self {
            security_descriptor,
            blob: FWP_BYTE_BLOB {
                size: security_descriptor_len,
                data: security_descriptor as *mut u8,
            },
        })
    }
}

impl Drop for UserMatchCondition {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                LocalFree(self.security_descriptor as HLOCAL);
            }
        }
    }
}

/// Ensures the persistent Codex WFP provider exists.
fn ensure_provider(engine: HANDLE) -> Result<()> {
    let provider_name = to_wide(OsStr::new(PROVIDER_NAME));
    let provider_description = to_wide(OsStr::new(PROVIDER_DESCRIPTION));
    let provider = FWPM_PROVIDER0 {
        providerKey: PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: provider_name.as_ptr() as *mut _,
            description: provider_description.as_ptr() as *mut _,
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };

    let result = unsafe { FwpmProviderAdd0(engine, &provider, null_mut()) };
    ensure_success_or(result, "FwpmProviderAdd0", &[FWP_E_ALREADY_EXISTS as u32])?;
    verify_provider(engine)
}

/// Ensures the persistent Codex sublayer exists under the Codex provider.
fn ensure_sublayer(engine: HANDLE) -> Result<()> {
    let sublayer_name = to_wide(OsStr::new(SUBLAYER_NAME));
    let sublayer_description = to_wide(OsStr::new(SUBLAYER_DESCRIPTION));
    let provider_key = PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: sublayer_name.as_ptr() as *mut _,
            description: sublayer_description.as_ptr() as *mut _,
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        weight: SUBLAYER_WEIGHT,
    };

    let result = unsafe { FwpmSubLayerAdd0(engine, &sublayer, null_mut()) };
    ensure_success_or(result, "FwpmSubLayerAdd0", &[FWP_E_ALREADY_EXISTS as u32])?;
    verify_sublayer(engine)
}

/// Adds one blocking WFP filter from the static filter spec list.
fn add_filter(
    engine: HANDLE,
    spec: &FilterSpec,
    namespace: WindowsSandboxPolicyNamespace,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let filter_name = to_wide(OsStr::new(spec.name(namespace)));
    let filter_description = to_wide(OsStr::new(spec.description));
    let mut filter_conditions = build_conditions(spec.conditions, user_condition);
    let provider_key = PROVIDER_KEY;
    let filter = FWPM_FILTER0 {
        filterKey: spec.key(namespace),
        displayData: FWPM_DISPLAY_DATA0 {
            name: filter_name.as_ptr() as *mut _,
            description: filter_description.as_ptr() as *mut _,
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        layerKey: spec.layer_key,
        subLayerKey: SUBLAYER_KEY,
        weight: empty_value(),
        numFilterConditions: filter_conditions.len() as u32,
        filterCondition: filter_conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
            Anonymous: FWPM_ACTION0_0 {
                filterType: zero_guid(),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };

    let mut filter_id = 0_u64;
    let result = unsafe { FwpmFilterAdd0(engine, &filter, null_mut(), &mut filter_id) };
    ensure_success(result, &format!("FwpmFilterAdd0({})", spec.name(namespace)))
}

/// Converts our compact condition specs into WFP filter conditions.
fn build_conditions(
    specs: &[ConditionSpec],
    user_condition: &UserMatchCondition,
) -> Vec<FWPM_FILTER_CONDITION0> {
    specs
        .iter()
        .map(|spec| match spec {
            ConditionSpec::User => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_USER_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        sd: &user_condition.blob as *const _ as *mut _,
                    },
                },
            },
            ConditionSpec::Protocol(protocol) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint8: *protocol },
                },
            },
            ConditionSpec::RemotePort(port) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
                },
            },
        })
        .collect()
}

/// Deletes an old copy of a filter before re-adding it.
fn delete_filter_if_present(engine: HANDLE, key: &GUID) -> Result<()> {
    let result = unsafe { FwpmFilterDeleteByKey0(engine, key) };
    ensure_success_or(
        result,
        "FwpmFilterDeleteByKey0",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )
}

fn ensure_success(result: u32, operation: &str) -> Result<()> {
    ensure_success_or(result, operation, &[])
}

fn ensure_success_or(result: u32, operation: &str, allowed: &[u32]) -> Result<()> {
    if result == 0 || allowed.contains(&result) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{operation} failed: {}",
            format_error_code(result)
        ))
    }
}

fn format_error_code(result: u32) -> String {
    format!("0x{result:08X}")
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        Anonymous: unsafe { zeroed() },
    }
}

fn zero_guid() -> GUID {
    GUID::from_u128(0)
}

#[cfg(test)]
mod tests {
    use super::Engine;
    use super::FILTER_SPECS;
    use super::delete_filter_if_present;
    use super::install_wfp_filters_for_account_in_namespace;
    use super::verify_wfp_filters_for_account_in_namespace;
    use crate::policy_namespace::WindowsSandboxPolicyNamespace;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

    #[test]
    fn policy_namespace_filter_keys_are_disjoint() {
        let keys = [
            WindowsSandboxPolicyNamespace::Codex,
            WindowsSandboxPolicyNamespace::McpConsole,
        ]
        .into_iter()
        .flat_map(|namespace| FILTER_SPECS.iter().map(move |spec| spec.key(namespace)))
        .map(|key| (key.data1, key.data2, key.data3, key.data4))
        .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), FILTER_SPECS.len() * 2);
    }

    #[test]
    fn policy_namespace_filter_names_are_disjoint() {
        let names = [
            WindowsSandboxPolicyNamespace::Codex,
            WindowsSandboxPolicyNamespace::McpConsole,
        ]
        .into_iter()
        .flat_map(|namespace| FILTER_SPECS.iter().map(move |spec| spec.name(namespace)))
        .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), FILTER_SPECS.len() * 2);
    }

    #[test]
    fn standalone_filter_verification_rejects_a_missing_filter() {
        if std::env::var("MCP_CONSOLE_SANDBOX_NATIVE_WINDOWS_TESTS").as_deref() != Ok("1") {
            return;
        }

        let _lease = crate::policy_lease::acquire_mcp_console_sandbox_policy_lease()
            .expect("acquire standalone policy lease");
        let namespace = WindowsSandboxPolicyNamespace::McpConsole;
        let account = namespace.offline_username();
        install_wfp_filters_for_account_in_namespace(account, namespace)
            .expect("install standalone WFP filters");
        verify_wfp_filters_for_account_in_namespace(account, namespace)
            .expect("verify installed standalone WFP filters");

        let engine = Engine::open().expect("open WFP engine");
        delete_filter_if_present(engine.handle, &FILTER_SPECS[0].key(namespace))
            .expect("delete one standalone WFP filter");
        drop(engine);
        let missing = verify_wfp_filters_for_account_in_namespace(account, namespace)
            .expect_err("missing standalone WFP filter must fail verification");
        let restored = install_wfp_filters_for_account_in_namespace(account, namespace);
        assert!(
            restored.is_ok(),
            "restore standalone WFP filters: {restored:?}"
        );
        assert!(
            missing
                .to_string()
                .contains(FILTER_SPECS[0].name(namespace)),
            "{missing:#}"
        );
    }
}
