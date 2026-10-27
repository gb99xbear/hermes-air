// hermes-plugin-host - Plugin system with WASM, native, and Python support
use hermes_protocol::Platform;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("Plugin not found: {0}")]
    NotFound(String),
    #[error("Plugin load failed: {0}")]
    LoadFailed(String),
    #[error("Plugin execution failed: {0}")]
    ExecutionFailed(String),
    #[error("WASM error: {0}")]
    Wasm(String),
    #[error("Python error: {0}")]
    Python(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub plugin_type: PluginType,
    pub entry_point: String,
    pub permissions: Vec<PluginPermission>,
    pub config_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    Wasm,
    Native,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPermission {
    Network,
    FileSystem,
    Process,
    Platform(Platform),
    Memory,
    Crypto,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    async fn initialize(&mut self, _config: serde_json::Value) -> Result<(), PluginError>;
    async fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, PluginError>;
    async fn shutdown(&mut self) -> Result<(), PluginError>;
}

pub struct WasmPlugin {
    manifest: PluginManifest,
    _instance: Option<wasmtime::Instance>,
    _store: Option<wasmtime::Store<()>>,
}

impl WasmPlugin {
    pub async fn load(wasm_path: &PathBuf, manifest: PluginManifest) -> Result<Self, PluginError> {
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::from_file(&engine, wasm_path)
            .map_err(|e| PluginError::Wasm(e.to_string()))?;
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &module, &[])
            .map_err(|e| PluginError::Wasm(e.to_string()))?;
        
        Ok(Self {
            manifest,
            _instance: Some(instance),
            _store: Some(store),
        })
    }
}

#[async_trait]
impl Plugin for WasmPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn initialize(&mut self, _config: serde_json::Value) -> Result<(), PluginError> {
        // Call WASM init function if exists
        Ok(())
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, PluginError> {
        // Call WASM execute function
        Ok(serde_json::json!({"status": "ok"}))
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

pub struct NativePlugin {
    manifest: PluginManifest,
    _library: Option<libloading::Library>,
}

impl NativePlugin {
    pub async fn load(lib_path: &PathBuf, manifest: PluginManifest) -> Result<Self, PluginError> {
        let library = unsafe { libloading::Library::new(lib_path) }
            .map_err(|e| PluginError::LoadFailed(e.to_string()))?;
        
        Ok(Self {
            manifest,
            _library: Some(library),
        })
    }
}

#[async_trait]
impl Plugin for NativePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn initialize(&mut self, _config: serde_json::Value) -> Result<(), PluginError> {
        // Call native init function
        Ok(())
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, PluginError> {
        // Call native execute function
        Ok(serde_json::json!({"status": "ok"}))
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(feature = "python")]
pub struct PythonPlugin {
    manifest: PluginManifest,
    module: Option<pyo3::Py<pyo3::PyModule>>,
}

#[cfg(feature = "python")]
impl PythonPlugin {
    pub async fn load(py_path: &PathBuf, manifest: PluginManifest) -> Result<Self, PluginError> {
        Python::with_gil(|py| {
            let module = pyo3::PyModule::from_code(py, &std::fs::read_to_string(py_path).unwrap(), "", "")
                .map_err(|e| PluginError::Python(e.to_string()))?;
            Ok(Self {
                manifest,
                module: Some(module.into()),
            })
        })
    }
}

#[cfg(feature = "python")]
#[async_trait]
impl Plugin for PythonPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn initialize(&mut self, _config: serde_json::Value) -> Result<(), PluginError> {
        Python::with_gil(|py| {
            if let Some(module) = &self.module {
                module.call_method1(py, "initialize", (config,))
                    .map_err(|e| PluginError::Python(e.to_string()))?;
            }
            Ok(())
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, PluginError> {
        Python::with_gil(|py| {
            if let Some(module) = &self.module {
                let result = module.call_method1(py, "execute", (input,))
                    .map_err(|e| PluginError::Python(e.to_string()))?;
                Ok(result.extract().unwrap_or(serde_json::json!({})))
            } else {
                Ok(serde_json::json!({}))
            }
        })
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

pub struct PluginHost {
    plugins: Arc<RwLock<HashMap<String, Box<dyn Plugin>>>>,
    manifests: Arc<RwLock<HashMap<String, PluginManifest>>>,
    _wasm_engine: wasmtime::Engine,
    plugin_dirs: Vec<PathBuf>,
    wasm_dir: Option<PathBuf>,
    python_enabled: bool,
}

impl PluginHost {
    pub fn new(
        plugin_dirs: Vec<PathBuf>,
        wasm_dir: Option<PathBuf>,
        python_enabled: bool,
    ) -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            manifests: Arc::new(RwLock::new(HashMap::new())),
            _wasm_engine: wasmtime::Engine::default(),
            plugin_dirs,
            wasm_dir,
            python_enabled,
        }
    }

    pub async fn load_all(&self) -> Result<(), PluginError> {
        // Load from plugin directories
        for dir in &self.plugin_dirs {
            self.load_from_dir(dir).await?;
        }

        // Load WASM plugins
        if let Some(wasm_dir) = &self.wasm_dir {
            self.load_wasm_plugins(wasm_dir).await?;
        }

        // Load Python plugins
        if self.python_enabled {
            self.load_python_plugins().await?;
        }

        Ok(())
    }

    async fn load_from_dir(&self, dir: &PathBuf) -> Result<(), PluginError> {
        if !dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(dir).await
            .map_err(|e| PluginError::LoadFailed(e.to_string()))?;

        while let Some(entry) = entries.next_entry().await
            .map_err(|e| PluginError::LoadFailed(e.to_string()))? {
            
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "json") {
                let content = tokio::fs::read_to_string(&path).await
                    .map_err(|e| PluginError::LoadFailed(e.to_string()))?;
                let manifest: PluginManifest = serde_json::from_str(&content)
                    .map_err(|e| PluginError::Serialization(e))?;
                
                self.manifests.write().await.insert(manifest.name.clone(), manifest.clone());

                // Load based on type
                match manifest.plugin_type {
                    PluginType::Wasm => {
                        let wasm_path = path.with_extension("wasm");
                        if wasm_path.exists() {
                            let plugin = WasmPlugin::load(&wasm_path, manifest).await?;
                            self.plugins.write().await.insert(plugin.manifest().name.clone(), Box::new(plugin));
                        }
                    }
                    PluginType::Native => {
                        let lib_path = path.with_extension(if cfg!(windows) { "dll" } else { "so" });
                        if lib_path.exists() {
                            let plugin = NativePlugin::load(&lib_path, manifest).await?;
                            self.plugins.write().await.insert(plugin.manifest().name.clone(), Box::new(plugin));
                        }
                    }
                    PluginType::Python => {
                        #[cfg(feature = "python")]
                        {
                            let plugin = PythonPlugin::load(&path, manifest).await?;
                            self.plugins.write().await.insert(plugin.manifest().name.clone(), Box::new(plugin));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn load_wasm_plugins(&self, _dir: &PathBuf) -> Result<(), PluginError> {
        // Similar to load_from_dir but for .wasm files
        Ok(())
    }

    async fn load_python_plugins(&self) -> Result<(), PluginError> {
        // Load Python plugins from plugin_dirs
        Ok(())
    }

    pub async fn execute(&self, name: &str, input: serde_json::Value) -> Result<serde_json::Value, PluginError> {
        let plugins = self.plugins.read().await;
        if let Some(plugin) = plugins.get(name) {
            plugin.execute(input).await
        } else {
            Err(PluginError::NotFound(name.to_string()))
        }
    }

    pub async fn list_plugins(&self) -> Vec<PluginManifest> {
        self.manifests.read().await.values().cloned().collect()
    }

    pub async fn unload(&self, name: &str) -> Result<(), PluginError> {
        let mut plugins = self.plugins.write().await;
        if let Some(mut plugin) = plugins.remove(name) {
            plugin.shutdown().await?;
        }
        self.manifests.write().await.remove(name);
        Ok(())
    }
}
