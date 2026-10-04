#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeMcpServer {
    pub name: String,
    pub url: String,
    pub environment_variable: Option<String>,
    pub tools: Vec<NativeMcpTool>,
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeMcpTool {
    pub name: String,
    pub exposed_name: String,
}
