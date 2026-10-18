//! Every word the console renders, in English and Simplified Chinese.
//!
//! Two const tables rather than one file per language: adding a string is a
//! compile error until both languages carry it, so a page cannot render half a
//! translation. Runtime data (model names, provider messages) is not translated.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// The console defaults to English; only an explicit zh selects Chinese.
    pub fn from_param(value: Option<&str>) -> Lang {
        match value {
            Some("zh") => Lang::Zh,
            _ => Lang::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Lang::En => &EN,
            Lang::Zh => &ZH,
        }
    }
}

pub struct Strings {
    pub kicker: &'static str,
    pub title_suffix: &'static str,
    pub live: &'static str,
    pub nav_board: &'static str,
    pub nav_usage: &'static str,
    pub nav_requests: &'static str,
    pub nav_prices: &'static str,
    pub nav_config: &'static str,
    pub nav_keys: &'static str,
    pub nav_doctor: &'static str,
    pub theme_to_night: &'static str,
    pub theme_to_day: &'static str,
    pub confirm_title: &'static str,
    pub confirm_cancel: &'static str,
    pub confirm_accept: &'static str,
    pub footer_loopback: &'static str,
    pub footer_config: &'static str,
    pub board_title: &'static str,
    pub board_eyebrow: &'static str,
    pub col_account: &'static str,
    pub col_covered: &'static str,
    pub col_metered: &'static str,
    pub col_resets: &'static str,
    pub col_state: &'static str,
    pub wallet: &'static str,
    pub credits: &'static str,
    pub stale: &'static str,
    pub back: &'static str,
    pub not_probed: &'static str,
    pub no_accounts: &'static str,
    pub used_of_cap: &'static str,
    pub config_title: &'static str,
    pub config_eyebrow: &'static str,
    pub config_live: &'static str,
    pub config_restart: &'static str,
    pub config_rawnote: &'static str,
    pub config_fingerprint: &'static str,
    pub config_last_applied: &'static str,
    pub config_none: &'static str,
    pub config_schedules: &'static str,
    pub section_server: &'static str,
    pub section_policy: &'static str,
    pub section_providers: &'static str,
    pub section_routes: &'static str,
    pub section_prices: &'static str,
    pub no_routes: &'static str,
    pub no_prices: &'static str,
    pub no_providers: &'static str,
}

pub static EN: Strings = Strings {
    kicker: "Light AI Gateway — Operator Console",
    title_suffix: "lgw console",
    live: "LIVE",
    nav_board: "Board",
    nav_usage: "Usage",
    nav_requests: "Requests",
    nav_prices: "Prices",
    nav_config: "Config",
    nav_keys: "Keys",
    nav_doctor: "Doctor",
    theme_to_night: "Night",
    theme_to_day: "Day",
    confirm_title: "Confirm",
    confirm_cancel: "Cancel",
    confirm_accept: "Continue",
    footer_loopback: "loopback only · no login",
    footer_config: "config",
    board_title: "Accounts & allowance",
    board_eyebrow: "01 — Pool",
    col_account: "Account",
    col_covered: "Covered",
    col_metered: "Metered",
    col_resets: "Resets",
    col_state: "State",
    wallet: "wallet",
    credits: "credits",
    stale: "stale",
    back: "back",
    not_probed: "No account has been probed yet. The board fills in after the first probe; an empty board never means an empty pool.",
    no_accounts: "No account is configured. Add a provider and an account in the config file, then reload.",
    used_of_cap: "used of cap",
    config_title: "Configuration",
    config_eyebrow: "02 — Config",
    config_live: "applies live",
    config_restart: "restart required",
    config_rawnote: "Read-only raw TOML. Literal secrets are redacted.",
    config_fingerprint: "content fingerprint",
    config_last_applied: "last applied",
    config_none: "not set",
    config_schedules: "schedules",
    section_server: "Server",
    section_policy: "Policy",
    section_providers: "Providers & accounts",
    section_routes: "Routes",
    section_prices: "Prices",
    no_routes: "No route is configured. A client model name resolves to nothing until one is added.",
    no_prices: "No price is configured. Unpriced models are charged at the deliberately conservative fallback.",
    no_providers: "No provider is configured.",
};

pub static ZH: Strings = Strings {
    kicker: "Light AI Gateway — 运维控制台",
    title_suffix: "lgw 控制台",
    live: "在线",
    nav_board: "看板",
    nav_usage: "用量",
    nav_requests: "请求",
    nav_prices: "价格",
    nav_config: "配置",
    nav_keys: "密钥",
    nav_doctor: "诊断",
    theme_to_night: "夜间",
    theme_to_day: "日间",
    confirm_title: "确认",
    confirm_cancel: "取消",
    confirm_accept: "继续",
    footer_loopback: "仅 loopback · 无登录",
    footer_config: "配置",
    board_title: "账号与额度",
    board_eyebrow: "01 — 池子",
    col_account: "账号",
    col_covered: "Covered",
    col_metered: "Metered",
    col_resets: "重置",
    col_state: "状态",
    wallet: "钱包",
    credits: "credits",
    stale: "stale",
    back: "回来",
    not_probed: "还没有任何账号被探测过。首次探测后看板会自动填上；空看板从来不表示池子是空的。",
    no_accounts: "还没有配置账号。先在配置文件里加上 provider 和 account，然后重新加载。",
    used_of_cap: "已用 / 上限",
    config_title: "配置",
    config_eyebrow: "02 — 配置",
    config_live: "立即生效",
    config_restart: "需要重启",
    config_rawnote: "只读原始 TOML。字面密钥已打码。",
    config_fingerprint: "内容指纹",
    config_last_applied: "最近应用",
    config_none: "未设置",
    config_schedules: "档位表",
    section_server: "服务",
    section_policy: "策略",
    section_providers: "Provider 与账号",
    section_routes: "路由",
    section_prices: "价格",
    no_routes: "还没有配置任何路由。加上之前，客户端模型名解析不到东西。",
    no_prices: "还没有配置任何价格。未定价的模型按刻意定高的默认价计费。",
    no_providers: "还没有配置任何 provider。",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_language_falls_back_to_english() {
        assert_eq!(Lang::from_param(None), Lang::En);
        assert_eq!(Lang::from_param(Some("fr")), Lang::En);
        assert_eq!(Lang::from_param(Some("zh")), Lang::Zh);
    }

    #[test]
    fn the_two_catalogs_are_actually_different() {
        assert_ne!(EN.board_title, ZH.board_title);
        assert_ne!(EN.nav_config, ZH.nav_config);
        assert_ne!(Lang::En.code(), Lang::Zh.code());
    }
}
