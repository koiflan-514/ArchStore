//! 在线翻译（project.md §7.2 第 4 层）：默认关闭，开启后按用户选择把描述发到第三方服务。
//!
//! **实测结论（2026-09-10，决定默认方案）**：
//! - Google 免费端点（translate_a/single?client=gtx）在本网络**不可达**（连接超时）。
//! - LibreTranslate 的公共实例已全部失效：libretranslate.com 返回 403（Cloudflare），
//!   translate.terraprint.co 502，lt.vern.cc 连接超时。因此它只能作为
//!   **用户自建/自配端点**存在，不能当默认。
//! - MyMemory 官方免费接口无需 key，实测稳定可用，是唯一可用的默认方案。
//!   但它有**硬限制：单次 q 最多 500 字符**，超出直接返回
//!   responseStatus=403 + "QUERY LENGTH LIMIT EXCEEDED"，
//!   而软件描述通常远超 500 字符，所以必须分块翻译再拼接。
//!
//! 硬性规则（§7.2）：
//! 1. 默认关闭；启用时才发送文本。
//! 2. 译文必须标注"机器翻译"，绝不冒充上游元数据。
//! 3. 24 小时缓存；翻译失败静默回退原文，不弹错误。
//! 4. 一次只翻译可见条目的描述（详情页），禁止批量翻译列表页。

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cache::{Cache, ttl};
use crate::config::{TranslationApi, TranslationConfig};
use crate::error::{CoreError, CoreResult};
use crate::net::{CancelToken, HttpClient, RetryPolicy};

/// MyMemory 的硬限制（实测：超过就 403）。留出余量取 480。
pub const MYMEMORY_MAX_CHARS: usize = 480;
/// 一次翻译最多分多少块（防止把整本书发出去）。
pub const MAX_CHUNKS: usize = 20;

/// 参与翻译的服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    MyMemory,
    LibreTranslate,
}

impl ProviderKind {
    /// 展示名（也用于"机器翻译（…）"标注）。
    pub fn label(&self) -> &'static str {
        match self {
            ProviderKind::MyMemory => "MyMemory",
            ProviderKind::LibreTranslate => "LibreTranslate",
        }
    }
}

/// 一次翻译的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Translation {
    pub text: String,
    /// 实际使用的服务（用于 UI 标注）
    pub provider: String,
    /// 目标语言
    pub target: String,
    /// 是否来自 24 小时缓存
    #[serde(default)]
    pub cached: bool,
    /// 是否只翻译了一部分（分块数触顶）
    #[serde(default)]
    pub truncated: bool,
}

impl Translation {
    /// 面向 UI 的标注文案（绝不冒充上游元数据）。
    pub fn label(&self) -> String {
        format!("机器翻译（{}）", self.provider)
    }
}

/// MyMemory 的响应体。
#[derive(Debug, Deserialize)]
struct MyMemoryResponse {
    #[serde(rename = "responseData")]
    response_data: Option<MyMemoryData>,
    #[serde(rename = "responseStatus")]
    response_status: Option<serde_json::Value>,
    #[serde(rename = "responseDetails")]
    response_details: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MyMemoryData {
    #[serde(rename = "translatedText")]
    translated_text: Option<String>,
}

/// LibreTranslate 的响应体。
#[derive(Debug, Deserialize)]
struct LibreResponse {
    #[serde(rename = "translatedText")]
    translated_text: Option<String>,
}

/// 翻译客户端。同样的 key 会命中 24 小时缓存，并对并发请求做单飞。
pub struct TranslateClient {
    http: Arc<HttpClient>,
    cache: Arc<Cache>,
    gate: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for TranslateClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslateClient").finish_non_exhaustive()
    }
}

impl TranslateClient {
    pub fn new(http: Arc<HttpClient>, cache: Arc<Cache>) -> Self {
        Self {
            http,
            cache,
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// 按配置翻译一段文本。
    ///
    /// 失败时返回 Err，由调用方静默回退原文（§7.2 规则 3）。
    pub async fn translate(
        &self,
        text: &str,
        target: &str,
        cfg: &TranslationConfig,
        cancel: &CancelToken,
    ) -> CoreResult<Translation> {
        if !cfg.auto_translate {
            return Err(CoreError::Unsupported("在线翻译未启用".into()));
        }
        let text = text.trim();
        if text.is_empty() {
            return Err(CoreError::Unsupported("没有可翻译的文本".into()));
        }
        let kind = match cfg.api {
            TranslationApi::None => {
                return Err(CoreError::Unsupported("未选择翻译服务".into()));
            }
            TranslationApi::MyMemory => ProviderKind::MyMemory,
            TranslationApi::LibreTranslate => ProviderKind::LibreTranslate,
        };

        // 缓存 key 必须包含服务与目标语言：换端点/换语言不能命中旧译文
        let key = format!(
            "{}:{}:{}",
            kind.label(),
            target,
            crate::cache::key::encode(text)
        );
        let _guard = self.gate.lock().await;
        let entry = self
            .cache
            .get_or_fetch("translate", &key, ttl::TRANSLATE, || async {
                let (translated, truncated) = match kind {
                    ProviderKind::MyMemory => self.via_mymemory(text, target, cancel).await?,
                    ProviderKind::LibreTranslate => {
                        self.via_libretranslate(text, target, cfg, cancel).await?
                    }
                };
                Ok(Translation {
                    text: translated,
                    provider: kind.label().to_string(),
                    target: target.to_string(),
                    cached: false,
                    truncated,
                })
            })
            .await?;
        Ok(entry.map(|t| Translation { cached: true, ..t }).value)
    }

    /// MyMemory：分块 + 逐块翻译 + 拼接。
    async fn via_mymemory(
        &self,
        text: &str,
        target: &str,
        cancel: &CancelToken,
    ) -> CoreResult<(String, bool)> {
        let lang = mymemory_lang(target);
        let chunks = split_for_translation(text, MYMEMORY_MAX_CHARS, MAX_CHUNKS);
        let truncated = chunks.truncated;
        let mut out: Vec<String> = Vec::with_capacity(chunks.parts.len());
        let langpair = format!("en|{lang}");

        for part in &chunks.parts {
            cancel.check()?;
            let url = format!(
                "https://api.mymemory.translated.net/get?q={}&langpair={}",
                percent_encode(part),
                percent_encode(&langpair)
            );
            let resp: MyMemoryResponse = self
                .http
                .get_json(
                    &url,
                    cancel,
                    RetryPolicy {
                        attempts: 2,
                        base: Duration::from_secs(1),
                    },
                )
                .await
                .map_err(|e| CoreError::Network {
                    url: "mymemory".into(),
                    cause: e.user_message(),
                })?;

            // 配额/长度超限都通过 responseStatus 表达，而不是 HTTP 状态码
            let status = resp
                .response_status
                .as_ref()
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(200);
            let details = resp.response_details.clone().unwrap_or_default();
            if status != 200 {
                return Err(if details.contains("QUERY LENGTH LIMIT") {
                    CoreError::Unsupported("文本超过免费翻译接口的长度限制".into())
                } else if details.contains("MYMEMORY WARNING") || details.contains("YOU USED ALL") {
                    CoreError::RateLimited { retry_after: 3600 }
                } else {
                    CoreError::Network {
                        url: "mymemory".into(),
                        cause: format!("服务返回 status={status} {details}"),
                    }
                });
            }

            let translated = resp
                .response_data
                .and_then(|d| d.translated_text)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.contains("QUERY LENGTH LIMIT"))
                .ok_or_else(|| CoreError::Parse {
                    context: "MyMemory 响应".into(),
                    raw_head: format!("status={status}"),
                })?;
            out.push(translated);
        }
        // 分块之间用空行拼接，保持段落感
        Ok((out.join("\n\n"), truncated))
    }

    /// LibreTranslate（用户自建端点）：POST /translate。
    async fn via_libretranslate(
        &self,
        text: &str,
        target: &str,
        cfg: &TranslationConfig,
        cancel: &CancelToken,
    ) -> CoreResult<(String, bool)> {
        let endpoint = cfg.api_endpoint.trim().trim_end_matches('/');
        if endpoint.is_empty() {
            return Err(CoreError::Config("未配置翻译端点".into()));
        }
        if !endpoint.starts_with("https://")
            && !endpoint.starts_with("http://localhost")
            && !endpoint.starts_with("http://127.0.0.1")
        {
            return Err(CoreError::Config("翻译端点必须是 https 或本机地址".into()));
        }
        let url = format!("{endpoint}/translate");
        let body = serde_json::json!({
            "q": text,
            "source": "en",
            "target": libretranslate_lang(target),
            "format": "text",
        });
        let client = self.http.client().await;
        let resp: LibreResponse = crate::net::post_json(&client, &url, &body, cancel).await?;
        let translated = resp
            .translated_text
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::Parse {
                context: "LibreTranslate 响应".into(),
                raw_head: String::new(),
            })?;
        Ok((translated, false))
    }
}

/// 分块结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunks {
    pub parts: Vec<String>,
    /// 是否因为超过 MAX_CHUNKS 而被截断
    pub truncated: bool,
}

/// 把长文本切成每块不超过 max 个字符的片段。
///
/// 优先在段落边界切，其次句子边界，最后词边界；
/// 这样译文不会在句子中间断裂（拼回去读起来才通顺）。
pub fn split_for_translation(text: &str, max: usize, max_chunks: usize) -> Chunks {
    let max = max.max(1);
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut truncated = false;

    'outer: for paragraph in text.split_inclusive("\n\n") {
        for sentence in split_sentences(paragraph) {
            for piece in hard_split(&sentence, max) {
                if piece.trim().is_empty() {
                    continue;
                }
                if current.chars().count() + piece.chars().count() <= max {
                    current.push_str(&piece);
                } else {
                    if !current.trim().is_empty() {
                        if parts.len() >= max_chunks {
                            truncated = true;
                            break 'outer;
                        }
                        parts.push(std::mem::take(&mut current));
                    }
                    current.push_str(&piece);
                }
            }
        }
    }
    if !truncated && !current.trim().is_empty() {
        if parts.len() >= max_chunks {
            truncated = true;
        } else {
            parts.push(current);
        }
    }
    if parts.is_empty() && !text.trim().is_empty() {
        parts.push(text.chars().take(max).collect());
    }
    Chunks { parts, truncated }
}

/// 在句末标点处切句，保留标点。
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        // 英文句末：. ! ?；中文句末：。！？；换行也作为边界
        let boundary = matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '；' | '\n');
        if !boundary {
            continue;
        }
        // 把句末空白并入本句：否则下一句会以空格开头，
        // 而且末尾会多出一个纯空白的"句子"（分块时被丢弃，看起来像丢字符）
        if c != '\n' {
            while chars
                .peek()
                .is_some_and(|n| n.is_whitespace() && *n != '\n')
            {
                current.push(chars.next().unwrap_or(' '));
            }
        }
        out.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 单句仍然超长时，按词边界硬切。
fn hard_split(sentence: &str, max: usize) -> Vec<String> {
    if sentence.chars().count() <= max {
        return vec![sentence.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in sentence.split_inclusive(char::is_whitespace) {
        if current.chars().count() + word.chars().count() > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if word.chars().count() > max {
            // 单个"词"就超长（例如没有空格的 CJK 长串）：按字符切
            let mut buf = String::new();
            for c in word.chars() {
                buf.push(c);
                if buf.chars().count() >= max {
                    out.push(std::mem::take(&mut buf));
                }
            }
            current.push_str(&buf);
            continue;
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// MyMemory 的中文语言代码（实测 zh / zh-CN / zh-Hans 都可用，用 zh-CN 最准确）。
fn mymemory_lang(target: &str) -> String {
    match target {
        "zh" | "zh-CN" | "zh-Hans" | "zh_CN" => "zh-CN".to_string(),
        "zh-TW" | "zh-Hant" | "zh_TW" => "zh-TW".to_string(),
        other => other.to_string(),
    }
}

/// LibreTranslate 的语言代码（简体用 zh，繁体用 zt）。
fn libretranslate_lang(target: &str) -> String {
    match target {
        "zh-CN" | "zh-Hans" | "zh_CN" | "zh" => "zh".to_string(),
        "zh-TW" | "zh-Hant" | "zh_TW" => "zt".to_string(),
        other => other.to_string(),
    }
}

/// 最小化的百分号编码（用于查询参数；只保留 RFC 3986 的 unreserved 字符）。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// 当前系统语言是否还需要翻译（中文环境下不必翻译）。
pub fn needs_translation(locale: &str) -> bool {
    !locale.to_ascii_lowercase().starts_with("zh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_matches_rfc3986() {
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(percent_encode("100%"), "100%25");
    }

    #[test]
    fn short_text_is_one_chunk() {
        let c = split_for_translation("Fast, Private & Safe Web Browser.", 480, 20);
        assert_eq!(c.parts.len(), 1);
        assert!(!c.truncated);
        assert_eq!(c.parts[0], "Fast, Private & Safe Web Browser.");
    }

    #[test]
    fn long_text_is_split_below_the_limit_and_loses_nothing() {
        // 模拟真实描述：多句英文，总长远超 480
        let para = "When it comes to your life online, you have a choice: accept the factory settings or put your privacy first. ";
        let text = para.repeat(10);
        assert!(text.chars().count() > 1000);
        let c = split_for_translation(&text, MYMEMORY_MAX_CHARS, MAX_CHUNKS);
        assert!(c.parts.len() > 1, "长文本必须分块");
        for p in &c.parts {
            assert!(
                p.chars().count() <= MYMEMORY_MAX_CHARS,
                "块长度 {} 超过上限",
                p.chars().count()
            );
        }
        // 所有块拼起来必须覆盖原文（按空白归一化比较：允许丢弃纯空白片段，
        // 但不允许丢失任何实际内容）
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let joined: String = c.parts.join("");
        assert_eq!(norm(&joined), norm(&text), "分块不得丢失内容");
        assert!(
            text.split_whitespace().count() == joined.split_whitespace().count(),
            "词数必须一致"
        );
        assert!(!c.truncated);
    }

    #[test]
    fn chunks_never_exceed_limit_without_punctuation() {
        let text = "a".repeat(3000);
        let c = split_for_translation(&text, 400, 20);
        for p in &c.parts {
            assert!(p.chars().count() <= 400);
        }
        assert_eq!(c.parts.len(), 8, "3000 / 400 = 8 块");
        assert_eq!(c.parts.concat().chars().count(), 3000);
    }

    #[test]
    fn punctuation_free_cjk_is_split_by_characters() {
        let text = "中".repeat(1000);
        let c = split_for_translation(&text, 300, 20);
        for p in &c.parts {
            assert!(p.chars().count() <= 300, "{}", p.chars().count());
        }
        assert_eq!(c.parts.concat().chars().count(), 1000);
    }

    #[test]
    fn max_chunks_marks_truncated() {
        let text = "Sentence one. ".repeat(500);
        let c = split_for_translation(&text, 100, 3);
        assert_eq!(c.parts.len(), 3);
        assert!(c.truncated, "超过块数上限必须标记 truncated");
    }

    #[test]
    fn empty_input_is_handled() {
        assert!(split_for_translation("", 480, 20).parts.is_empty());
        assert!(split_for_translation("   ", 480, 20).parts.is_empty());
    }

    #[test]
    fn sentence_splitting_keeps_punctuation_and_trailing_space() {
        let s = split_sentences("One. Two! Three? Four");
        assert_eq!(s.len(), 4);
        // 句末空白并入本句，避免下一句以空格开头
        assert_eq!(s[0], "One. ");
        assert_eq!(s[1], "Two! ");
        assert_eq!(s[2], "Three? ");
        assert_eq!(s[3], "Four");
        // 结尾的空白不应产生纯空白句子
        let s = split_sentences("Done. ");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], "Done. ");
        let s = split_sentences("A\nB");
        assert_eq!(s, vec!["A\n".to_string(), "B".to_string()]);
    }

    #[test]
    fn language_codes_map_correctly() {
        assert_eq!(mymemory_lang("zh-CN"), "zh-CN");
        assert_eq!(mymemory_lang("zh"), "zh-CN");
        assert_eq!(mymemory_lang("zh_TW"), "zh-TW");
        assert_eq!(mymemory_lang("ja"), "ja");
        assert_eq!(libretranslate_lang("zh-CN"), "zh");
        assert_eq!(libretranslate_lang("zh-TW"), "zt");
    }

    #[test]
    fn translation_label_marks_machine_translation() {
        let t = Translation {
            text: "你好".into(),
            provider: "MyMemory".into(),
            target: "zh-CN".into(),
            cached: false,
            truncated: false,
        };
        assert_eq!(t.label(), "机器翻译（MyMemory）");
        assert!(t.label().contains("机器翻译"));
    }

    #[test]
    fn needs_translation_only_for_non_chinese() {
        assert!(!needs_translation("zh_CN.UTF-8"));
        assert!(!needs_translation("zh-TW"));
        assert!(needs_translation("en_US.UTF-8"));
        assert!(needs_translation("ja_JP.UTF-8"));
    }

    #[test]
    fn mymemory_response_deserializes() {
        let json = r#"{"responseData":{"translatedText":"快速、私密和安全的网络浏览器","match":0.85},
            "quotaFinished":false,"responseDetails":"","responseStatus":200}"#;
        let r: MyMemoryResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(
            r.response_data.and_then(|d| d.translated_text).as_deref(),
            Some("快速、私密和安全的网络浏览器")
        );
        assert_eq!(r.response_status.and_then(|v| v.as_u64()), Some(200));
    }

    #[test]
    fn mymemory_quota_error_shape_is_recognized() {
        // 实测：超过 500 字符时返回的正是这个
        let json = r#"{"responseData":{"translatedText":"QUERY LENGTH LIMIT EXCEEDED. MAX ALLOWED QUERY : 500 CHARS"},
            "responseStatus":403,"responseDetails":"QUERY LENGTH LIMIT EXCEEDED. MAX ALLOWED QUERY : 500 CHARS"}"#;
        let r: MyMemoryResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.response_status.and_then(|v| v.as_u64()), Some(403));
        assert!(
            r.response_details
                .unwrap_or_default()
                .contains("QUERY LENGTH LIMIT")
        );
    }
}
