//! Config parsing + credential-resolution tests. Split out of `config.rs`
//! so the schema file stays inside the god-file budget.

use super::*;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sunmao-im-cfg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn parses_minimal_config() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[{"kind":"telegram","enabled":true,"token_env":"TG_TOKEN"}]}"#,
    )
    .unwrap();
    assert_eq!(cfg.dm_policy, DmPolicy::Pairing);
    assert_eq!(cfg.dm_scope, DmScope::Main);
    assert_eq!(cfg.enabled_specs().count(), 1);
}

#[test]
fn disabled_channel_skips() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[{"kind":"telegram","enabled":false,"token_env":"T"}]}"#,
    )
    .unwrap();
    assert_eq!(cfg.enabled_specs().count(), 0);
}

#[test]
fn missing_is_none_not_err() {
    assert!(
        ChannelsConfig::load(Path::new("/nonexistent/channels.json"))
            .unwrap()
            .is_none()
    );
}

/// A kind that is not telegram lands on its own variant and keeps its
/// scoped policy — the generic path every new adapter rides.
#[test]
fn new_kind_parses_with_its_own_scope() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[{"kind":"test","enabled":true,
            "dm_policy":"open","allowlist":["test:7"]}]}"#,
    )
    .unwrap();
    let spec = cfg.specs().next().unwrap();
    assert_eq!(spec.kind_name(), "test");
    assert!(spec.enabled());
    let scoped = spec.scoped().unwrap();
    assert_eq!(scoped.dm_policy, Some(DmPolicy::Open));
    assert_eq!(scoped.allowlist, ["test:7"]);
    assert_eq!(cfg.enabled_specs().count(), 1);
}

/// A config written for a newer binary must parse and be skipped —
/// never an error, never an enabled channel.
#[test]
fn unknown_kind_parses_and_skips() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[{"kind":"future_channel","enabled":true,"whatever":1}]}"#,
    )
    .unwrap();
    let spec = cfg.specs().next().unwrap();
    assert!(matches!(spec, ChannelSpec::Unknown));
    assert_eq!(spec.kind_name(), "unknown");
    assert!(!spec.enabled());
    assert!(spec.scoped().is_none());
    assert_eq!(cfg.enabled_specs().count(), 0);
}

/// The four new blocks parse with their own policy surface, unknown
/// fields inside a block are ignored, and the DingTalk API base carries
/// its default. This is what lets batch 2 land an adapter without a
/// config change.
#[test]
fn new_platform_blocks_parse() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[
            {"kind":"feishu","enabled":true,"app_id":"cli_x",
             "app_secret_env":"FS_SECRET","region":"lark_global",
             "owner":"feishu:ou_1","dm_policy":"allowlist",
             "allowlist":["feishu:ou_1"],"ui_only_field":true},
            {"kind":"qq","enabled":true,"app_id":"1024",
             "app_secret_file":"D:/secrets/qq.txt"},
            {"kind":"dingtalk","enabled":false,"corp_id":"c","client_id":"k",
             "client_secret_env":"DT_SECRET","robot_code":"r"},
            {"kind":"wechat","enabled":false,"bot_token_env":"WX_TOKEN"}
        ]}"#,
    )
    .unwrap();
    let specs: Vec<&ChannelSpec> = cfg.specs().collect();
    assert_eq!(specs[0].kind_name(), "feishu");
    assert_eq!(specs[1].kind_name(), "qq");
    assert_eq!(specs[2].kind_name(), "dingtalk");
    assert_eq!(specs[3].kind_name(), "wechat");
    assert!(specs[0].enabled() && specs[1].enabled());
    assert!(!specs[2].enabled() && !specs[3].enabled());

    let ChannelSpec::Feishu(f) = specs[0] else {
        panic!("feishu block did not land on its variant");
    };
    assert_eq!(f.region, FeishuRegion::LarkGlobal);
    assert_eq!(f.app_id, "cli_x");
    let scoped = specs[0].scoped().unwrap();
    assert_eq!(scoped.dm_policy, Some(DmPolicy::Allowlist));
    assert_eq!(scoped.allowlist, ["feishu:ou_1"]);

    let ChannelSpec::Dingtalk(d) = specs[2] else {
        panic!("dingtalk block did not land on its variant");
    };
    assert_eq!(d.api_base_url, "https://api.dingtalk.com/v1.0");
    assert_eq!(d.client_secret_env.as_deref(), Some("DT_SECRET"));

    let ChannelSpec::Wechat(w) = specs[3] else {
        panic!("wechat block did not land on its variant");
    };
    assert_eq!(w.bot_token_env.as_deref(), Some("WX_TOKEN"));
}

/// Region defaults to the CN domain when the block says nothing.
#[test]
fn feishu_region_defaults_to_cn() {
    let cfg = serde_json::from_str::<ChannelsConfig>(
        r#"{"channels":[{"kind":"feishu","enabled":true,"app_id":"cli_x"}]}"#,
    )
    .unwrap();
    let ChannelSpec::Feishu(f) = cfg.specs().next().unwrap() else {
        panic!("not a feishu block");
    };
    assert_eq!(f.region, FeishuRegion::FeishuCn);
}

/// The credential convention: env beats file, file alone works, a named
/// but unset env var is an error (never a silent fall back to a stale
/// file), and neither reference is a named config error. Telegram's token
/// and the new platforms share one resolver, so one test covers the shape.
#[test]
fn secret_env_wins_over_file() {
    let file = temp_path("secret.txt");
    std::fs::write(&file, "from-file\n").unwrap();
    let spec = FeishuSpec {
        app_secret_env: Some("SUNMAO_TEST_FEISHU_SECRET".into()),
        app_secret_file: Some(file.clone()),
        ..empty_feishu()
    };
    unsafe { std::env::set_var("SUNMAO_TEST_FEISHU_SECRET", "from-env\n") };
    assert_eq!(spec.app_secret().unwrap(), "from-env");
    unsafe { std::env::remove_var("SUNMAO_TEST_FEISHU_SECRET") };
    let err = spec.app_secret().unwrap_err().to_string();
    assert!(err.contains("app_secret_env"), "{err}");

    // file-only — the reference that actually resolves
    let file_only = FeishuSpec {
        app_secret_file: Some(file.clone()),
        ..empty_feishu()
    };
    assert_eq!(file_only.app_secret().unwrap(), "from-file");

    // neither reference set → the config explains itself
    let none = FeishuSpec { ..empty_feishu() };
    let err = none.app_secret().unwrap_err().to_string();
    assert!(err.contains("app_secret_env or app_secret_file"), "{err}");
    std::fs::remove_dir_all(file.parent().unwrap()).ok();
}

/// Every platform's resolver names its own field pair — the error is the
/// operator's only clue when a secret reference is wrong.
#[test]
fn each_platform_names_its_own_credential_field() {
    let qq = QqSpec::default_for_test();
    assert!(
        qq.app_secret()
            .unwrap_err()
            .to_string()
            .contains("app_secret")
    );
    let dt = DingtalkSpec::default_for_test();
    assert!(
        dt.client_secret()
            .unwrap_err()
            .to_string()
            .contains("client_secret")
    );
    let wx = WechatSpec::default_for_test();
    assert!(
        wx.bot_token()
            .unwrap_err()
            .to_string()
            .contains("bot_token")
    );
    let tg = TelegramSpec {
        token_env: None,
        token_file: None,
        poll_timeout_secs: 30,
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
        enabled: true,
    };
    assert!(tg.token().unwrap_err().to_string().contains("token_env"));
}

fn empty_feishu() -> FeishuSpec {
    FeishuSpec {
        enabled: false,
        app_id: String::new(),
        app_secret_env: None,
        app_secret_file: None,
        region: FeishuRegion::FeishuCn,
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
    }
}

impl QqSpec {
    fn default_for_test() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret_env: None,
            app_secret_file: None,
            owner: None,
            dm_policy: None,
            allowlist: Vec::new(),
        }
    }
}

impl DingtalkSpec {
    fn default_for_test() -> Self {
        Self {
            enabled: false,
            corp_id: String::new(),
            client_id: String::new(),
            client_secret_env: None,
            client_secret_file: None,
            robot_code: String::new(),
            api_base_url: default_dingtalk_api_base(),
            owner: None,
            dm_policy: None,
            allowlist: Vec::new(),
        }
    }
}

impl WechatSpec {
    fn default_for_test() -> Self {
        Self {
            enabled: false,
            bot_token_env: None,
            bot_token_file: None,
            owner: None,
            dm_policy: None,
            allowlist: Vec::new(),
        }
    }
}
