use super::FILTER_SPECS;
use super::PROVIDER_KEY;
use super::SUBLAYER_KEY;
use crate::WindowsSandboxProduct;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

#[test]
fn product_namespaces_keep_all_installed_wfp_objects_disjoint() {
    let source_keys = [PROVIDER_KEY, SUBLAYER_KEY]
        .into_iter()
        .chain(FILTER_SPECS.iter().map(|spec| spec.key))
        .collect::<Vec<_>>();
    let keys = [WindowsSandboxProduct::Codex, WindowsSandboxProduct::Console]
        .into_iter()
        .flat_map(|product| source_keys.iter().map(move |key| product.wfp_key(*key)))
        .map(|key| (key.data1, key.data2, key.data3, key.data4))
        .collect::<BTreeSet<_>>();
    assert_eq!(keys.len(), source_keys.len() * 2);
}

#[test]
fn product_selection_cannot_change_after_initialization() {
    WindowsSandboxProduct::Codex
        .initialize()
        .expect("default product");
    assert!(WindowsSandboxProduct::Console.initialize().is_err());
    assert_eq!(
        WindowsSandboxProduct::current(),
        WindowsSandboxProduct::Codex
    );
}
