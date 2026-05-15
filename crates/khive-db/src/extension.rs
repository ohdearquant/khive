use std::sync::Once;

static INIT: Once = Once::new();

pub fn ensure_extensions_loaded() {
    INIT.call_once(|| {
        // sqlite-vec extension will be loaded here when vector support is enabled.
        // For now this is a placeholder — the extension is loaded on-demand
        // by StorageBackend::vectors() when the feature is available.
    });
}
