use crate::config::Config;
use crate::oci::pull_and_extract_oci_image;
use anyhow::Result;
use extism::{Manifest, Plugin, Wasm};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{Error as McpError, ServerHandler, model::*};

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct PluginService {
    config: Config,
    plugins: Arc<RwLock<HashMap<String, Plugin>>>,
    tool_plugin_map: Arc<RwLock<HashMap<String, String>>>,
}

impl PluginService {
    pub async fn new(config: Config) -> Result<Self> {
        let service = Self {
            config,
            plugins: Arc::new(RwLock::new(HashMap::new())),
            tool_plugin_map: Arc::new(RwLock::new(HashMap::new())),
        };

        service.load_plugins().await?;
        Ok(service)
    }

    async fn load_plugins(&self) -> Result<()> {
        for plugin_cfg in &self.config.plugins {
            let wasm_content = if plugin_cfg.path.starts_with("http") {
                reqwest::get(&plugin_cfg.path)
                    .await?
                    .bytes()
                    .await?
                    .to_vec()
            } else if plugin_cfg.path.starts_with("oci") {
                // ref should be like oci://tuananh/qr-code
                let image_reference = plugin_cfg.path.strip_prefix("oci://").unwrap();
                let target_file_path = "/plugin.wasm";
                let mut hasher = Sha256::new();
                hasher.update(image_reference);
                let hash = hasher.finalize();
                let short_hash = &hex::encode(hash)[..7];
                let cache_dir = dirs::cache_dir()
                    .map(|mut path| {
                        path.push("hyper-mcp");
                        path
                    })
                    .unwrap();
                std::fs::create_dir_all(&cache_dir)?;

                let local_output_path =
                    cache_dir.join(format!("{}-{}.wasm", plugin_cfg.name, short_hash));
                let local_output_path = local_output_path.to_str().unwrap();

                // Use the CLI flag to determine whether to skip signature verification
                let verify_signature = !self.config.insecure_skip_signature;

                if let Err(e) = pull_and_extract_oci_image(
                    image_reference,
                    target_file_path,
                    local_output_path,
                    verify_signature,
                )
                .await
                {
                    log::error!("Error pulling oci plugin: {}", e);
                    return Err(anyhow::anyhow!("Failed to pull OCI plugin: {}", e));
                }
                log::info!(
                    "cache plugin `{}` to : {}",
                    plugin_cfg.name,
                    local_output_path
                );
                tokio::fs::read(local_output_path).await?
            } else {
                tokio::fs::read(&plugin_cfg.path).await?
            };

            let mut manifest = Manifest::new([Wasm::data(wasm_content)]);
            if let Some(runtime_cfg) = &plugin_cfg.runtime_config {
                log::info!("runtime_cfg: {:?}", runtime_cfg);
                if let Some(hosts) = &runtime_cfg.allowed_hosts {
                    for host in hosts {
                        manifest = manifest.with_allowed_host(host);
                    }
                }
                if let Some(paths) = &runtime_cfg.allowed_paths {
                    for path in paths {
                        // path will be available in the plugin with exact same path
                        manifest = manifest.with_allowed_path(path.clone(), path.clone());
                    }
                }

                // Add plugin configurations if present
                if let Some(env_vars) = &runtime_cfg.env_vars {
                    for (key, value) in env_vars {
                        manifest = manifest.with_config_key(key, value);
                    }
                }
            }
            let mut plugin = Plugin::new(&manifest, [], true).unwrap();

            // Try to get tool information from the plugin and populate the cache
            if let Ok(result) = plugin.call::<&str, &str>("describe", "") {
                if let Ok(parsed) = serde_json::from_str::<ListToolsResult>(result) {
                    let mut cache = self.tool_plugin_map.write().await;
                    for tool in parsed.tools {
                        log::info!("Saving tool {}/{} to cache", plugin_cfg.name, tool.name);
                        // Check if the tool name already exists in another plugin
                        if let Some(existing_plugin) = cache.get(&tool.name.to_string()) {
                            if existing_plugin != &plugin_cfg.name {
                                log::error!(
                                    "Tool name collision detected: {} is provided by both {} and {} plugins",
                                    tool.name,
                                    existing_plugin,
                                    plugin_cfg.name
                                );
                                panic!(
                                    "Tool name collision detected: {} is provided by both {} and {} plugins",
                                    tool.name, existing_plugin, plugin_cfg.name
                                );
                            }
                        }
                        cache.insert(tool.name.to_string(), plugin_cfg.name.clone());
                    }
                }
            }

            let plugin_name = plugin_cfg.name.clone();
            self.plugins
                .write()
                .await
                .insert(plugin_name.clone(), plugin);
            log::info!("Loaded plugin {}", plugin_name);
        }
        Ok(())
    }
}

impl ServerHandler for PluginService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            server_info: Implementation {
                name: "hyper-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: ServerCapabilities::builder().enable_tools().build(),

            ..Default::default()
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let mut plugins = self.plugins.write().await;
        let tool_cache = self.tool_plugin_map.read().await;

        let tool_name = request.name.clone();
        let call_payload = json!({
            "params": request,
        });
        let json_string =
            serde_json::to_string(&call_payload).expect("Failed to serialize request");

        // Check if the tool exists in the cache
        if let Some(plugin_name) = tool_cache.get(&tool_name.to_string()) {
            if let Some(plugin) = plugins.get_mut(plugin_name) {
                return match plugin.call::<&str, &str>("call", &json_string) {
                    Ok(result) => match serde_json::from_str::<CallToolResult>(result) {
                        Ok(parsed) => Ok(parsed),
                        Err(e) => {
                            return Err(McpError::internal_error(
                                format!("Failed to deserialize data: {}", e),
                                None,
                            ));
                        }
                    },
                    Err(e) => {
                        return Err(McpError::internal_error(
                            format!("Failed to execute plugin {}: {}", plugin_name, e),
                            None,
                        ));
                    }
                };
            }
        }

        Err(McpError::method_not_found::<CallToolRequestMethod>())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        tracing::info!("got tools/list request {:?}", request);
        let mut plugins = self.plugins.write().await;
        let mut tool_cache = self.tool_plugin_map.write().await;

        let mut payload = ListToolsResult::default();

        // Clear the existing cache when listing tools
        tool_cache.clear();

        for (plugin_name, plugin) in plugins.iter_mut() {
            match plugin.call::<&str, &str>("describe", "") {
                Ok(result) => {
                    let parsed: ListToolsResult = serde_json::from_str(result).unwrap();

                    // Update the tool-to-plugin cache
                    for tool in &parsed.tools {
                        tool_cache.insert(tool.name.to_string(), plugin_name.clone());
                    }

                    payload.tools.extend(parsed.tools);
                }
                Err(e) => {
                    log::error!("tool {} describe() error: {}", plugin_name, e);
                }
            }
        }

        Ok(payload)
    }

    // fn list_tools(
    //     &self,
    //     _request: Option<PaginatedRequestParam>,
    //     _context: RequestContext<RoleServer>,
    // ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
    //     tracing::info!("got tools/list request {:?}", _request);
    //     std::future::ready(Ok(ListToolsResult::default()))
    // }

    fn initialize(
        &self,
        request: InitializeRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, McpError>> + Send + '_ {
        tracing::info!("got initialize request {:?}", request);
        std::future::ready(Ok(self.get_info()))
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<(), McpError>> + Send + '_ {
        tracing::info!("got ping request");
        std::future::ready(Ok(()))
    }

    fn on_initialized(&self) -> impl Future<Output = ()> + Send + '_ {
        tracing::info!("got initialized notification");
        std::future::ready(())
    }

    fn on_cancelled(
        &self,
        _notification: CancelledNotificationParam,
    ) -> impl Future<Output = ()> + Send + '_ {
        std::future::ready(())
    }

    fn on_progress(
        &self,
        _notification: ProgressNotificationParam,
    ) -> impl Future<Output = ()> + Send + '_ {
        std::future::ready(())
    }

    fn complete(
        &self,
        request: CompleteRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<CompleteResult, McpError>> + Send + '_ {
        tracing::info!("got complete request {:?}", request);
        std::future::ready(Err(McpError::method_not_found::<CompleteRequestMethod>()))
    }
}
