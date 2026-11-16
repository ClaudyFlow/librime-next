//! 字段级隐私开关（2026-11-09，照 phono 清单口径）。
//!
//! 三个**互相独立**的字段，缺省全开（老行为零回归）：
//!
//! | 配置键 | 关掉之后 |
//! |---|---|
//! | `translator/read_context` | 神经输入不带 `pre_commit_context`（不读上文） |
//! | `translator/enable_neural_inference` | 不跑神经预测 / 补位 / 重排，联想回退个人 n-gram |
//! | `translator/enable_learning` | 不写 `user_ngram` / `rerank_head` / pin 表（不学习、不落盘） |
//!
//! 宿主可再用 `WEASEL_PRIVACY=off|0|false` 一键三层全关（只读一次，与
//! `WEASEL_PREDICT` / `WEASEL_RERANK_HEAD` 同一套进程级开关约定）。
//!
//! 这层只回答「能不能」，不碰模型：模型缺席时由 `rescore::global()` 返回
//! `None` 走既有的空结果降级链，与此处的开关正交。

use std::sync::{OnceLock, RwLock};

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Privacy {
    pub context: bool,
    pub inference: bool,
    pub learning: bool,
}

impl Privacy {
    pub const ALL: Self = Self {
        context: true,
        inference: true,
        learning: true,
    };

    const NONE: Self = Self {
        context: false,
        inference: false,
        learning: false,
    };
}

/// 单个布尔键：只认真/假、1/0；**缺键 = true**（默认开，不影响既有部署）。
fn flag(config: &Value, key: &str) -> bool {
    config
        .pointer(key)
        .and_then(|value| value.as_bool().or_else(|| value.as_i64().map(|n| n != 0)))
        .unwrap_or(true)
}

pub fn from_config(config: &Value) -> Privacy {
    Privacy {
        context: flag(config, "/translator/read_context"),
        inference: flag(config, "/translator/enable_neural_inference"),
        learning: flag(config, "/translator/enable_learning"),
    }
}

fn env_forced_off(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim),
        Some(v) if v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false")
    )
}

fn forced_off() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| env_forced_off(std::env::var("WEASEL_PRIVACY").ok().as_deref()))
}

fn state() -> &'static RwLock<Privacy> {
    static STATE: OnceLock<RwLock<Privacy>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(Privacy::ALL))
}

/// 切方案 / 重新装配组件时按新方案配置刷新。
pub fn set_from_config(config: &Value) {
    if let Ok(mut guard) = state().write() {
        *guard = from_config(config);
    }
}

pub fn get() -> Privacy {
    if forced_off() {
        return Privacy::NONE;
    }
    state()
        .read()
        .map(|guard| *guard)
        .unwrap_or(Privacy::ALL)
}

pub fn context_enabled() -> bool {
    get().context
}

pub fn inference_enabled() -> bool {
    get().inference
}

pub fn learning_enabled() -> bool {
    get().learning
}

/// 单测用：把进程级状态恢复成全开（不改环境变量）。
#[cfg(test)]
pub fn reset_for_test() {
    if let Ok(mut guard) = state().write() {
        *guard = Privacy::ALL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn three_fields_are_independent() {
        let p = from_config(&json!({
            "translator": {
                "read_context": false,
                "enable_neural_inference": true,
                "enable_learning": 0
            }
        }));
        assert_eq!(
            p,
            Privacy {
                context: false,
                inference: true,
                learning: false
            }
        );
        assert_eq!(from_config(&json!({})), Privacy::ALL, "缺键一律全开");
        assert_eq!(
            from_config(&json!({ "translator": { "read_context": 1 } })),
            Privacy::ALL,
            "数字 1 等同 true"
        );
    }

    #[test]
    fn env_off_is_a_third_layer() {
        assert!(env_forced_off(Some("off")));
        assert!(env_forced_off(Some("OFF")), "大小写不敏感");
        assert!(env_forced_off(Some("0")));
        assert!(env_forced_off(Some("false")));
        assert!(!env_forced_off(Some("on")));
        assert!(!env_forced_off(None));
    }
}
