use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::{Config, ToolsConfig};
use crate::napcat::NapcatClient;
use crate::openai::OpenAiCompatClient;
use crate::util;

const TOOL_REQUEST_START: &str = "【TOOL_REQUEST】";
const TOOL_REQUEST_END: &str = "【END_TOOL_REQUEST】";

#[derive(Debug, Clone)]
pub struct ToolRuntimeContext {
    pub group_id: String,
    pub current_image_urls: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root_dir: PathBuf,
    config_path: PathBuf,
    napcat: NapcatClient,
    http: Client,
}

#[derive(Debug, Deserialize)]
pub struct ToolRequest {
    pub tool: String,
    #[serde(default)]
    pub self_assessed_safe: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub notify_text: String,
    #[serde(default)]
    pub delete_after_send: bool,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub old_text: String,
    #[serde(default)]
    pub new_text: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub image_url: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl ToolExecutor {
    pub fn new(root_dir: PathBuf, config_path: PathBuf, napcat: NapcatClient) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            root_dir,
            config_path,
            napcat,
            http,
        })
    }

    pub async fn execute(
        &self,
        config: &Config,
        runtime: &ToolRuntimeContext,
        request: ToolRequest,
    ) -> anyhow::Result<String> {
        if !config.tools.enabled {
            bail!("工具执行未启用");
        }
        if !request.self_assessed_safe {
            bail!("模型未确认该操作安全，拒绝执行");
        }
        let tool_name = request.tool.trim().to_string();
        let result = match tool_name.as_str() {
            "run_python" => self.run_python(&config.tools, &request).await,
            "edit_text_file" => self.edit_text_file(&config.tools, &request).await,
            "send_local_file" => self.send_local_file(&config.tools, runtime, &request).await,
            "fetch_web_page" => self.fetch_web_page(&config.tools, &request).await,
            "read_image" => self.read_image(config, runtime, &request).await,
            "shell_command" => self.shell_command(&config.tools, &request).await,
            _ => bail!("不支持的工具：{tool_name}"),
        };
        let audit_status = if result.is_ok() { "ok" } else { "error" };
        let audit_result = match &result {
            Ok(text) => trim_text(text, 1200),
            Err(error) => trim_text(&error.to_string(), 1200),
        };
        self.append_audit_log(
            &config.tools,
            json!({
                "time": util::now_iso(),
                "tool": tool_name,
                "groupId": runtime.group_id,
                "reason": request.reason,
                "status": audit_status,
                "result": audit_result
            }),
        ).await;
        result
    }

    async fn run_python(&self, tools: &ToolsConfig, request: &ToolRequest) -> anyhow::Result<String> {
        let code = request.code.trim();
        if code.is_empty() {
            bail!("run_python 缺少 code");
        }
        ensure_python_code_safe(code)?;
        let temp_dir = self.resolve_path(&tools.temp_dir);
        fs::create_dir_all(&temp_dir).await?;
        let file_path = temp_dir.join(format!("tool-python-{}-{}.py", std::process::id(), now_millis()));
        fs::write(&file_path, code).await?;
        let output = timeout(
            Duration::from_millis(tools.execution_timeout_ms.max(1000)),
            Command::new("python3")
                .arg("-I")
                .arg(&file_path)
                .output(),
        )
        .await
        .context("python 执行超时")?
        .context("python 执行失败")?;
        let _ = fs::remove_file(&file_path).await;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!("python 执行失败：{}", trim_text(&stderr, 1200));
        }
        Ok(format!(
            "python 执行完成。\nstdout:\n{}\n{}",
            trim_text(&stdout, tools.fetch_max_chars),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\nstderr:\n{}", trim_text(&stderr, 1200))
            }
        ))
    }

    async fn edit_text_file(&self, tools: &ToolsConfig, request: &ToolRequest) -> anyhow::Result<String> {
        let target = self.validate_editable_path(tools, &request.file_path)?;
        let mode = request.mode.trim();
        if mode.is_empty() {
            bail!("edit_text_file 缺少 mode");
        }
        let existing = fs::read_to_string(&target).await.unwrap_or_default();
        let next = match mode {
            "overwrite" => request.content.clone(),
            "append" => format!("{existing}{}", request.content),
            "replace_all" => {
                if request.old_text.is_empty() {
                    bail!("replace_all 需要 old_text");
                }
                if !existing.contains(&request.old_text) {
                    bail!("目标文本不存在，拒绝 replace_all");
                }
                existing.replace(&request.old_text, &request.new_text)
            }
            _ => bail!("不支持的编辑模式：{mode}"),
        };
        ensure_text_edit_safe(&target, &next)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&target, next).await?;
        Ok(format!("文本文件已更新：{}", target.display()))
    }

    async fn send_local_file(
        &self,
        tools: &ToolsConfig,
        runtime: &ToolRuntimeContext,
        request: &ToolRequest,
    ) -> anyhow::Result<String> {
        let target = self.validate_send_file_path(tools, &request.file_path)?;
        if !target.exists() {
            bail!("待发送文件不存在：{}", target.display());
        }
        if !target.is_file() {
            bail!("只允许发送普通文件：{}", target.display());
        }
        let file_name = if request.file_name.trim().is_empty() {
            target.file_name().and_then(|item| item.to_str()).unwrap_or("tool-output.bin").to_string()
        } else {
            request.file_name.trim().to_string()
        };
        self.napcat
            .send_local_file_to_group(&runtime.group_id, &target, &file_name, request.notify_text.trim())
            .await?;
        if request.delete_after_send || self.is_under_path(&target, &self.resolve_path(&tools.temp_dir)) {
            let _ = fs::remove_file(&target).await;
        }
        Ok(format!("文件已发送到群 {}：{}", runtime.group_id, target.display()))
    }

    async fn fetch_web_page(&self, tools: &ToolsConfig, request: &ToolRequest) -> anyhow::Result<String> {
        let url = request.url.trim();
        if url.is_empty() {
            bail!("fetch_web_page 缺少 url");
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("只允许抓取 http/https 页面");
        }
        let response = self.http.get(url).send().await?;
        if !response.status().is_success() {
            bail!("抓取网页失败：HTTP {}", response.status());
        }
        let text = response.text().await?;
        Ok(format!(
            "网页抓取成功：{}\n{}",
            url,
            trim_text(&text, tools.fetch_max_chars)
        ))
    }

    async fn shell_command(&self, tools: &ToolsConfig, request: &ToolRequest) -> anyhow::Result<String> {
        let program = request.program.trim();
        if program.is_empty() {
            bail!("shell_command 缺少 program");
        }
        ensure_shell_args_safe(program, &request.args, tools, &self.root_dir)?;
        let output = timeout(
            Duration::from_millis(tools.execution_timeout_ms.max(1000)),
            Command::new(program).args(&request.args).output(),
        )
        .await
        .context("shell_command 执行超时")?
        .context("shell_command 执行失败")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!("shell_command 执行失败：{}", trim_text(&stderr, 1200));
        }
        Ok(format!(
            "命令执行完成：{} {}\nstdout:\n{}\n{}",
            program,
            request.args.join(" "),
            trim_text(&stdout, tools.fetch_max_chars),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\nstderr:\n{}", trim_text(&stderr, 1200))
            }
        ))
    }

    async fn read_image(
        &self,
        config: &Config,
        runtime: &ToolRuntimeContext,
        request: &ToolRequest,
    ) -> anyhow::Result<String> {
        let image_url = if !request.image_url.trim().is_empty() {
            request.image_url.trim().to_string()
        } else if runtime.current_image_urls.len() == 1 {
            runtime.current_image_urls[0].clone()
        } else {
            bail!("read_image 缺少 image_url，且当前消息没有唯一可用图片");
        };
        if !image_url.starts_with("http://") && !image_url.starts_with("https://") {
            bail!("read_image 只允许读取 http/https 图片");
        }
        let question = if request.question.trim().is_empty() {
            "请描述这张图片里最重要的信息，优先识别文字、界面、截图中的报错、人物动作和上下文。回答简洁自然。".to_string()
        } else {
            request.question.trim().to_string()
        };
        let mut client = OpenAiCompatClient::new(config.ai.clone())?;
        let text = client
            .complete_with_image_url(
                &question,
                &image_url,
                if config.ai.vision_model.trim().is_empty() {
                    None
                } else {
                    Some(config.ai.vision_model.trim())
                },
                Some(config.ai.temperature),
                Some(config.ai.max_tokens),
            )
            .await?;
        Ok(format!("读图完成：{}\n{}", image_url, trim_text(&text, config.tools.fetch_max_chars)))
    }

    async fn append_audit_log(&self, tools: &ToolsConfig, payload: Value) {
        let audit_path = self.resolve_path(&tools.audit_log_path);
        if let Some(parent) = audit_path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        let line = format!("{}\n", payload);
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
            .await
        {
            use tokio::io::AsyncWriteExt;
            let _ = file.write_all(line.as_bytes()).await;
        }
    }

    fn resolve_path(&self, target: &str) -> PathBuf {
        let path = PathBuf::from(target.trim());
        if path.is_absolute() {
            path
        } else {
            self.root_dir.join(path)
        }
    }

    fn is_under_path(&self, target: &Path, root: &Path) -> bool {
        let normalized_target = normalize_path(target);
        let normalized_root = normalize_path(root);
        normalized_target.starts_with(&normalized_root)
    }

    fn validate_editable_path(&self, tools: &ToolsConfig, target: &str) -> anyhow::Result<PathBuf> {
        let path = self.resolve_path(target);
        ensure_path_not_protected(tools, &path, &self.config_path)?;
        Ok(path)
    }

    fn validate_send_file_path(&self, tools: &ToolsConfig, target: &str) -> anyhow::Result<PathBuf> {
        let path = self.resolve_path(target);
        ensure_path_not_protected(tools, &path, &self.config_path)?;
        Ok(path)
    }
}

pub fn extract_tool_request(text: &str) -> Option<ToolRequest> {
    let source = text.trim();
    let start = source.find(TOOL_REQUEST_START)?;
    let end = source.find(TOOL_REQUEST_END)?;
    if end <= start {
        return None;
    }
    let raw = source[start + TOOL_REQUEST_START.len()..end].trim();
    serde_json::from_str::<ToolRequest>(raw).ok()
}

pub fn build_tool_system_prompt(config: &Config) -> String {
    if !config.tools.enabled {
        return String::new();
    }
    format!(
        concat!(
            "你拥有一组受限工具，但必须同时满足两个条件才可调用：\n",
            "1. 你自己判断该动作必要且安全\n",
            "2. 程序的硬限制黑名单未命中\n\n",
            "如果要调用工具，你必须只输出一段工具 JSON，格式如下：\n",
            "{start}{{\"tool\":\"run_python|edit_text_file|send_local_file|fetch_web_page|read_image|shell_command\",\"self_assessed_safe\":true,\"reason\":\"为什么安全且必要\",...}}{end}\n\n",
            "规则：\n",
            "- 只有在纯文本回答做不到时才调用工具。\n",
            "- 如果不需要工具，就直接正常回复文本，或输出【SKIP】。\n",
            "- 禁止请求修改任何配置文件、服务文件、数据库、密钥文件。\n",
            "- run_python 只能写纯计算/文本处理脚本，禁止 import os、subprocess、socket、shutil、pathlib、ctypes，也禁止 open()。\n",
            "- edit_text_file 默认可改普通文本文件，但一旦命中配置、状态、数据库、密钥或系统敏感路径，会被程序拒绝。\n",
            "- send_local_file 默认可发送现有文件，但一旦命中配置、状态、数据库、密钥或系统敏感路径，会被程序拒绝。\n",
            "- fetch_web_page 只能抓取 http/https 文本页面。\n",
            "- read_image 用于读取当前消息或指定 URL 的图片内容；如果用户发的是截图、报错图、界面图、照片，且理解图片对回答有帮助，应优先调用它。\n",
            "- shell_command 默认允许普通系统命令；但危险程序、危险参数、解释器绕过、敏感路径访问会被程序拒绝。\n",
            "- 当用户询问实时状态，如内存、磁盘、进程、端口、目录内容时，应优先考虑 shell_command，而不是说自己拿不到状态。\n",
            "- 一次只允许请求一个工具。\n",
            "- 工具执行结果会回到上下文，你再继续判断是否还需要下一步。\n",
            "- 最多工具轮数：{max_rounds}。"
        ),
        start = TOOL_REQUEST_START,
        end = TOOL_REQUEST_END,
        max_rounds = config.tools.max_rounds
    )
}

fn ensure_python_code_safe(code: &str) -> anyhow::Result<()> {
    let normalized = code.to_lowercase();
    let banned = [
        "import os",
        "from os",
        "import subprocess",
        "from subprocess",
        "import socket",
        "from socket",
        "import shutil",
        "from shutil",
        "import pathlib",
        "from pathlib",
        "import ctypes",
        "from ctypes",
        "open(",
        "__import__",
        "eval(",
        "exec(",
    ];
    if let Some(hit) = banned.iter().find(|item| normalized.contains(**item)) {
        bail!("python 代码命中禁用规则：{hit}");
    }
    Ok(())
}

fn ensure_text_edit_safe(path: &Path, content: &str) -> anyhow::Result<()> {
    if path
        .extension()
        .and_then(|item| item.to_str())
        .map(|item| item.eq_ignore_ascii_case("db"))
        .unwrap_or(false)
    {
        bail!("禁止编辑数据库文件");
    }
    let normalized = content.to_lowercase();
    if normalized.contains("sk-") || normalized.contains("bearer ") {
        bail!("禁止写入疑似密钥内容");
    }
    Ok(())
}

fn ensure_shell_args_safe(
    program: &str,
    args: &[String],
    tools: &ToolsConfig,
    root_dir: &Path,
) -> anyhow::Result<()> {
    let normalized_program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim()
        .to_lowercase();
    if let Some(hit) = tools
        .shell_blocked_programs
        .iter()
        .map(|item| item.trim().to_lowercase())
        .find(|item| !item.is_empty() && *item == normalized_program)
    {
        bail!("命令程序命中禁用规则：{hit}");
    }
    let joined = format!("{} {}", normalized_program, args.join(" ").to_lowercase());
    if let Some(hit) = tools
        .shell_blocked_tokens
        .iter()
        .map(|item| item.trim().to_lowercase())
        .find(|item| !item.is_empty() && joined.contains(item))
    {
        bail!("命令命中禁用规则：{hit}");
    }
    for arg in args {
        if let Some(path) = maybe_resolve_arg_path(root_dir, arg) {
            ensure_path_not_protected(tools, &path, root_dir)?;
        }
    }
    Ok(())
}

fn ensure_path_not_protected(tools: &ToolsConfig, path: &Path, config_path: &Path) -> anyhow::Result<()> {
    let normalized_path = normalize_path(path);
    let normalized_config = normalize_path(config_path);
    if normalized_path == normalized_config {
        bail!("禁止访问主配置文件：{}", path.display());
    }
    let rendered_path = normalized_path
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    if let Some(hit) = tools
        .protected_path_keywords
        .iter()
        .map(|item| item.trim().replace('\\', "/").to_lowercase())
        .find(|item| !item.is_empty() && rendered_path.contains(item))
    {
        bail!("目标路径命中敏感路径规则：{hit}");
    }
    if let Some(file_name) = normalized_path.file_name().and_then(|item| item.to_str()) {
        let file_name = file_name.to_lowercase();
        if let Some(hit) = tools
            .protected_file_names
            .iter()
            .map(|item| item.trim().to_lowercase())
            .find(|item| !item.is_empty() && *item == file_name)
        {
            bail!("目标文件命中敏感文件规则：{hit}");
        }
    }
    if let Some(ext) = normalized_path.extension().and_then(|item| item.to_str()) {
        let ext = ext.to_lowercase();
        if let Some(hit) = tools
            .protected_extensions
            .iter()
            .map(|item| item.trim().trim_start_matches('.').to_lowercase())
            .find(|item| !item.is_empty() && *item == ext)
        {
            bail!("目标文件扩展名命中敏感规则：.{hit}");
        }
    }
    Ok(())
}

fn maybe_resolve_arg_path(root_dir: &Path, arg: &str) -> Option<PathBuf> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('-') {
        return None;
    }
    if trimmed.contains('=') && !trimmed.starts_with('/') && !trimmed.starts_with("./") && !trimmed.starts_with("../") {
        return None;
    }
    let candidate = if let Some((_, value)) = trimmed.split_once('=') {
        value
    } else {
        trimmed
    };
    if candidate.is_empty() {
        return None;
    }
    let looks_like_path = candidate.starts_with('/')
        || candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.contains('/')
        || candidate.contains('\\');
    if !looks_like_path {
        return None;
    }
    let path = PathBuf::from(candidate);
    Some(if path.is_absolute() { path } else { root_dir.join(path) })
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn trim_text(text: &str, max_chars: usize) -> String {
    let collected = text.chars().take(max_chars).collect::<String>();
    if collected.chars().count() == text.chars().count() {
        collected
    } else {
        format!("{collected}\n...(已截断)")
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}
