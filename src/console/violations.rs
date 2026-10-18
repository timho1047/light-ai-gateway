//! Chinese wording for the configuration violations the validator can report.
//!
//! Each entry is a stable code from `ConfigViolation::code`; the English message
//! stays in the validator and is shown as technical detail for a Chinese page.
//! A code with no translation falls back to the English message, so a new check
//! can never render an empty error.

use super::copy::Lang;

/// The page-language headline for a violation code, or None for English (the
/// validator's own message is already English).
pub fn headline(lang: Lang, code: &str) -> Option<&'static str> {
    match lang {
        Lang::En => None,
        Lang::Zh => Some(match code {
            "soft_threshold_out_of_range" => "policy.soft_threshold 必须在 0.0 到 1.0 之间",
            "soft_threshold_not_below_hard_gate" => {
                "policy.soft_threshold 必须小于 1.0：这段余量是防止请求溢出到真钱消费的缓冲"
            }
            "max_retries_too_high" => "policy.max_retries 不能超过 8",
            "hard_margin_invalid" => "policy.hard_margin 必须是大于等于 0 的有限数",
            "drift_threshold_invalid" => "policy.drift_threshold 必须是大于等于 0 的有限数",
            "listen_requires_local_key" => {
                "非 loopback 监听地址必须配置 local_key，否则任何能访问该地址的人都能花掉池子的配额"
            }
            "no_providers" => "还没有配置任何 provider",
            "price_tier_name_empty" => "价格档位的名字不能为空：它是学到的价存储时用的键",
            "price_tier_name_duplicate" => {
                "同一个模型的两个价格档位不能重名：名字是学到的价的存储键"
            }
            "base_url_empty" => "base_url 不能为空",
            "provider_name_contains_slash" => "provider 名字不能包含斜杠",
            "url_not_absolute" => "base_url / probe_base_url 必须是绝对 URL",
            "url_scheme_not_http" => "base_url / probe_base_url 只能用 http 或 https",
            "url_missing_host" => "URL 没有主机名",
            "credit_multiplier_invalid" => "credit_multiplier 必须是大于 0 的有限数",
            "provider_has_no_accounts" => "这个 provider 还没有配置账号",
            "account_id_empty" => "账号 id 不能为空",
            "account_id_contains_slash" => "账号 id 不能包含斜杠（provider/account 是凭据库的键）",
            "route_unknown_provider" => "路由指向了不存在的 provider",
            "provider_prefix_alias_unknown_provider" => "前缀别名指向了不存在的 provider",
            "prices_unknown_provider" => "这个价格块的名字不是已配置的 provider",
            "price_model_name_empty" => "价格块里的模型名不能为空",
            "price_schedule_invalid" => "价格档位的时间窗口格式不合法",
            _ => "配置校验未通过（详见技术细节）",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_have_chinese_wording() {
        assert!(
            headline(Lang::Zh, "soft_threshold_out_of_range")
                .unwrap()
                .contains("0.0 到 1.0")
        );
        assert!(headline(Lang::Zh, "no_providers").is_some());
        assert!(headline(Lang::Zh, "made_up_code").is_some());
    }

    #[test]
    fn english_uses_the_validator_message() {
        assert!(headline(Lang::En, "soft_threshold_out_of_range").is_none());
    }
}
