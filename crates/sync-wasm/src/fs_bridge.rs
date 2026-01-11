//! JavaScript filesystem bridge for WASM.
//!
//! Implements the `FileSystem` trait by calling JavaScript callback functions
//! provided by the Obsidian plugin. Each callback is an async JS function that
//! returns a Promise, which we convert to a Rust Future.

use async_trait::async_trait;
use sync_core::fs::{FileEntry, FileStat, FileSystem, FsError, Result};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// JavaScript filesystem bridge.
///
/// Holds JS callback functions for filesystem operations. The TypeScript side
/// creates this with callbacks that wrap Obsidian's Vault API.
///
/// # Example (TypeScript side)
///
/// ```typescript
/// const bridge = new JsFileSystemBridge(
///   (path) => vault.adapter.readBinary(path),
///   (path, data) => vault.adapter.writeBinary(path, data),
///   // ... etc
/// );
/// ```
#[wasm_bindgen]
pub struct JsFileSystemBridge {
    read_fn: js_sys::Function,
    write_fn: js_sys::Function,
    list_fn: js_sys::Function,
    delete_fn: js_sys::Function,
    exists_fn: js_sys::Function,
    stat_fn: js_sys::Function,
    mkdir_fn: js_sys::Function,
}

#[wasm_bindgen]
impl JsFileSystemBridge {
    /// Create a new filesystem bridge with JS callback functions.
    ///
    /// All callbacks should be async functions (returning Promises).
    #[wasm_bindgen(constructor)]
    pub fn new(
        read_fn: js_sys::Function,
        write_fn: js_sys::Function,
        list_fn: js_sys::Function,
        delete_fn: js_sys::Function,
        exists_fn: js_sys::Function,
        stat_fn: js_sys::Function,
        mkdir_fn: js_sys::Function,
    ) -> Self {
        Self {
            read_fn,
            write_fn,
            list_fn,
            delete_fn,
            exists_fn,
            stat_fn,
            mkdir_fn,
        }
    }
}

/// Helper to call a JS function and await its Promise result.
async fn call_js_async(func: &js_sys::Function, args: &[JsValue]) -> std::result::Result<JsValue, JsValue> {
    let this = JsValue::NULL;
    
    // Build arguments array
    let js_args = js_sys::Array::new();
    for arg in args {
        js_args.push(arg);
    }
    
    // Call the function - it returns a Promise
    let promise = func.apply(&this, &js_args)?;
    
    // Convert Promise to Future and await
    JsFuture::from(js_sys::Promise::from(promise)).await
}

/// Convert a JS error to our FsError type.
fn js_err_to_fs_err(err: JsValue) -> FsError {
    let msg = err
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&err, &"message".into())
                .ok()
                .and_then(|v| v.as_string())
        })
        .unwrap_or_else(|| format!("{:?}", err));
    
    // Try to detect specific error types from the message
    if msg.contains("not found") || msg.contains("ENOENT") {
        FsError::NotFound(msg)
    } else if msg.contains("already exists") || msg.contains("EEXIST") {
        FsError::AlreadyExists(msg)
    } else if msg.contains("is a directory") || msg.contains("EISDIR") {
        FsError::IsDirectory(msg)
    } else {
        FsError::Io(msg)
    }
}

/// Represents file stat returned from JS.
#[derive(serde::Deserialize)]
struct JsFileStat {
    mtime: f64,
    size: f64,
    #[serde(rename = "isDir")]
    is_dir: bool,
}

/// Represents file entry returned from JS.
#[derive(serde::Deserialize)]
struct JsFileEntry {
    name: String,
    #[serde(rename = "isDir")]
    is_dir: bool,
}

#[async_trait(?Send)]
impl FileSystem for JsFileSystemBridge {
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let result = call_js_async(&self.read_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        // Result should be a Uint8Array
        let array = js_sys::Uint8Array::from(result);
        Ok(array.to_vec())
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        // Convert content to Uint8Array for JS
        let js_array = js_sys::Uint8Array::from(content);
        
        call_js_async(&self.write_fn, &[path.into(), js_array.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let result = call_js_async(&self.list_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        // Result should be an array of { name, isDir } objects
        let entries: Vec<JsFileEntry> = serde_wasm_bindgen::from_value(result)
            .map_err(|e| FsError::Io(format!("Failed to parse list result: {}", e)))?;
        
        Ok(entries
            .into_iter()
            .map(|e| FileEntry {
                name: e.name,
                is_dir: e.is_dir,
            })
            .collect())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        call_js_async(&self.delete_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let result = call_js_async(&self.exists_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        Ok(result.as_bool().unwrap_or(false))
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        let result = call_js_async(&self.stat_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        let js_stat: JsFileStat = serde_wasm_bindgen::from_value(result)
            .map_err(|e| FsError::Io(format!("Failed to parse stat result: {}", e)))?;
        
        Ok(FileStat {
            mtime_millis: js_stat.mtime as u64,
            size: js_stat.size as u64,
            is_dir: js_stat.is_dir,
        })
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        call_js_async(&self.mkdir_fn, &[path.into()])
            .await
            .map_err(js_err_to_fs_err)?;
        
        Ok(())
    }
}
