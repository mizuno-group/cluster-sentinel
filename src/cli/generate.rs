//! Generating a complete configuration file.
//!
//! An operator arriving with a fresh binary should not have to read a
//! reference manual to get a first file. `sentinel config init` writes every
//! setting at its default, annotated, so the file itself is the documentation:
//! what exists, what it is set to now, and what changing it would mean.
//!
//! Values come from [`Config::default`] and the probe catalog rather than
//! being typed out here, so a generated file cannot claim a default the binary
//! does not have.

use std::time::Duration;

use crate::config::{Config, DEFAULT_CONFIG_PATH, DEFAULT_STATE_DIR};
use crate::probes::catalog;

/// Which side of the deployment a generated file is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The controller: one per environment.
    Controller,
    /// An agent: one per monitored host.
    Agent,
}

impl Role {
    /// Parse a role name.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "controller" => Some(Role::Controller),
            "agent" => Some(Role::Agent),
            _ => None,
        }
    }

    /// The name used on the command line and in unit files.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Controller => "controller",
            Role::Agent => "agent",
        }
    }
}

/// The marker an operator must replace before the file is usable.
pub const PLACEHOLDER: &str = "CHANGE-ME";

fn duration(value: Duration) -> String {
    crate::time::format_duration(value)
}

/// Render a complete configuration file for `role`.
pub fn config_file(role: Role) -> String {
    let defaults = Config::default();
    let mut out = String::new();

    out.push_str(&header(role));
    out.push_str(&format!(
        "\nconfig_version = {}\n\n\
         # 同一クラスタの全 host で一致させること。\n\
         # 不一致の host は別クラスタとして扱われ、同じ名前でも別 entity になります。\n\
         environment = \"{PLACEHOLDER}-environment\"\n",
        defaults.config_version
    ));

    match role {
        Role::Controller => out.push_str(&controller_section(&defaults)),
        Role::Agent => out.push_str(&agent_section(&defaults)),
    }

    // Retention is the controller's database; an agent has no say in it.
    if role == Role::Controller {
        out.push_str(&retention_section(&defaults));
    }
    out.push_str(&probes_section());
    out.push_str(&tls_section(role));

    if role == Role::Controller {
        out.push_str(&notification_section());
        out.push_str(&inventory_section());
    } else {
        out.push_str(&agent_capabilities_section());
    }

    out
}

fn header(role: Role) -> String {
    let what = match role {
        Role::Controller => "controller（環境に 1 台）",
        Role::Agent => "agent（監視対象の各 host）",
    };
    format!(
        "\
# Cluster Sentinel — {what}
#
# `sentinel config init` が生成したファイルです。
# すべての設定が既定値のまま書き出してあります。
#
# 配置先: {DEFAULT_CONFIG_PATH}
# 検証:   sudo -u sentinel sentinel config check
#
# \"{PLACEHOLDER}\" を含む行だけが、必ず書き換えなければならない箇所です。
# それ以外はすべて既定値なので、変更したい行のコメントを外してください。
"
    )
}

fn controller_section(defaults: &Config) -> String {
    let c = &defaults.controller;
    format!(
        "\n\
# ===========================================================================
# Controller
# ===========================================================================
[controller]
# API の待ち受けアドレス。agent はここに報告します。
listen = \"{listen}\"

# inventory discovery の間隔。scontrol を実行し全 host を probe するため高価です。
inventory_interval = \"{inventory}\"

# 診断・相関・通知の間隔。保存済みデータを読むだけなので安価です。
# **障害発生から通知までの遅延を決めるのはこの値です。**
diagnosis_interval = \"{diagnosis}\"

# controller 自身も観測点として動作するか。
# firewall の内側にいて視界が偏る場合のみ false にしてください。
observe = {observe}

[database]
path = \"{db}\"

[peer_monitoring]
# 1 entity あたりに割り当てる observer 数。
# 2 未満だと到達性の診断ができません（1 つの視点では
# 「host が落ちた」と「その経路だけ切れた」を区別できないため）。
degree = {degree}

[discovery.slurm]
# Slurm クラスタなら true。node 一覧と状態を scontrol から取り込みます。
enabled = {slurm}
# scontrol が PATH にない場合のみ指定。
# scontrol_path = \"/opt/slurm/bin/scontrol\"
",
        listen = c.listen,
        inventory = duration(c.inventory_interval),
        diagnosis = duration(c.diagnosis_interval),
        observe = c.observe,
        db = defaults.database.path.display(),
        degree = defaults.peer_monitoring.degree,
        slurm = defaults.discovery.slurm.enabled,
    )
}

fn agent_section(defaults: &Config) -> String {
    format!(
        "\n\
# ===========================================================================
# Agent
# ===========================================================================
[agent]
# controller のアドレス。ここだけは必ず書き換えてください。
# TLS を使う場合は [tls] を設定すれば https:// として解釈されます。
controller_address = \"{PLACEHOLDER}-controller-host:7443\"

# controller に届かない間、観測結果を溜めておく場所。
spool_path = \"{spool}\"

# この agent の health endpoint。peer がここを見て
# 「agent だけ落ちた」と「host が落ちた」を区別します。
# 変更しても agent が自分で controller に報告するため、controller 側の追記は不要です。
listen = \"{listen}\"

# UI 上のグループ分けにのみ使われます。probe を有効化することはありません
# （probe は capability で決まります）。
roles = []

# ---------------------------------------------------------------------------
# この host に peer が到達するアドレス
#
# 未指定なら agent が自動検出します。ただし「どの NIC でクラスタ内通信を
# しているか」は site の事実であり、host を調べても分かりません。
# VLAN が複数ある、bridge がある、fabric が複数あるなら **必ず指定してください。**
#
# `sentinel doctor` が、報告されるアドレスと候補一覧、
# 曖昧な場合はその旨を表示します。
# ---------------------------------------------------------------------------
# NIC 名で指定（fleet 全体で同じ 1 行が使えるので推奨）
# interface = \"vlan20\"
#
# アドレスを直接指定（NAT 越しなど、host 自身から見えない場合）
# address = \"192.168.20.2\"

# SSH が 22 以外で、かつ /etc/ssh/sshd_config から読めない場合のみ指定。
# 通常は agent が sshd_config から自動検出します。
# ssh_port = 2222
",
        spool = std::path::Path::new(DEFAULT_STATE_DIR).join("spool.db").display(),
        listen = defaults.agent.listen,
    )
}

fn retention_section(defaults: &Config) -> String {
    let r = &defaults.retention;
    format!(
        "\n\
# ===========================================================================
# 記録の保持期間
#
# database は書き込み一方です。実測で host 1 台あたり 1 日約 170 MB 増えます。
# 既定値で prune は有効なので、host あたり約 2.4 GB で頭打ちになります。
#
# 各期間には duration のほか \"never\"（無期限保持）を指定できます。
#
# 以下は設定で緩められません:
#   * open な incident は年齢に関わらず削除されない
#   * 生存中の incident / diagnosis が参照する observation は削除されない
#   * 各 entity の直近 keep_per_entity 件は削除されない
# ===========================================================================
[retention]
enabled = {enabled}
interval = \"{interval}\"
observations = \"{observations}\"
keep_per_entity = {keep}
transitions = \"{transitions}\"
resolved_incidents = \"{incidents}\"
diagnoses = \"{diagnoses}\"
",
        enabled = r.enabled,
        interval = duration(r.interval),
        observations = r.observations,
        keep = r.keep_per_entity,
        transitions = r.transitions,
        incidents = r.resolved_incidents,
        diagnoses = r.diagnoses,
    )
}

fn probes_section() -> String {
    let mut out = String::from(
        "\n\
# ===========================================================================
# 監視頻度
#
# 以下はすべて **コンパイル時の既定値** です。行のコメントを外すと上書きされます。
# 書かれていない probe は既定のまま動くため、1 つだけ調整しても他には影響しません。
#
#   interval        実行間隔
#   timeout         1 回あたりの上限時間
#   max_outstanding 同一 target に対する同時実行数（**引き下げのみ可能**）
#   enabled         false にするとその probe は動きません
#
# 大規模クラスタで負荷を下げたい場合は interval を延ばしてください。
# ただし network.tcp を延ばすと host 障害の検出全体が遅くなります。
# ===========================================================================
",
    );

    for entry in catalog::catalog() {
        let d = &entry.definition;
        out.push_str(&format!("\n# {}\n", entry.description));
        if let Some(caution) = entry.caution {
            out.push_str(&format!("# 注意: {caution}\n"));
        }
        out.push_str(&format!("# [probes.\"{}\"]\n", d.id));
        out.push_str(&format!("# interval = \"{}\"\n", duration(d.interval)));
        out.push_str(&format!("# timeout = \"{}\"\n", duration(d.timeout)));
        if d.max_outstanding != u32::MAX {
            out.push_str(&format!("# max_outstanding = {}\n", d.max_outstanding));
        }
        out.push_str("# enabled = true\n");
    }

    out
}

fn tls_section(role: Role) -> String {
    let body = match role {
        Role::Controller => {
            "\
# [tls]
# cert = \"/etc/sentinel/tls/controller.crt\"
# key  = \"/etc/sentinel/tls/controller.key\"
#
# 以下を書くと client 証明書が「必須」になります（任意にはなりません）。
# cluster credential が漏洩しても耐えられる構成はこれだけです。
# client_ca = \"/etc/sentinel/tls/ca.crt\"
"
        }
        Role::Agent => {
            "\
# [tls]
# ca = \"/etc/sentinel/tls/ca.crt\"
#
# mutual TLS の場合。証明書は host ごとに発行してください。
# client_cert = \"/etc/sentinel/tls/agent.crt\"
# client_key  = \"/etc/sentinel/tls/agent.key\"
#
# controller を IP で指定し、証明書が名前しか持たない場合。
# server_name = \"controller.example\"
#
# PKI がまだ無い場合の暫定措置。TLS を装飾に変えます。
# insecure_skip_verify = true
"
        }
    };

    format!(
        "\n\
# ===========================================================================
# TLS
#
# cluster credential は bearer token です。平文で運用すると、
# 通信を読める者が全 agent になりすませます。
#
# 何も書かなければ平文 HTTP のまま動作します。
# 証明書は既存の PKI で発行してください（Sentinel は生成しません）。
# 手順: docs/DEPLOYMENT.md §9.6
# ===========================================================================
{body}"
    )
}

fn notification_section() -> String {
    String::from(
        "\n\
# ===========================================================================
# 通知
#
# 状態が変化したときだけ送信します。継続中の incident は繰り返し通知しません。
# ===========================================================================
# [notification]
# min_severity = \"warning\"          # \"info\" / \"warning\" / \"critical\"
#
# [[notification.webhooks]]
# name = \"ops\"
# url  = \"https://example.invalid/hooks/sentinel\"
",
    )
}

fn inventory_section() -> String {
    format!(
        "\n\
# ===========================================================================
# Slurm の外にある host と storage
#
# Slurm discovery では見つからないため、ここで宣言します。
# agent を入れる予定の host も、先に書いておいて構いません（merge されます）。
#
# 依存関係を書かないと、複数 node の storage 障害が
# SHARED_STORAGE_FAILURE（原因 = fileserver）ではなく
# 個別の NFS_CLIENT_FAILURE として報告されます。
# ===========================================================================
# [[entities]]
# type = \"host\"
# name = \"{PLACEHOLDER}-fileserver-1\"
# capabilities = [\"storage.nfs.server\"]
# # SSH が 22 以外の場合のみ
# # ports = {{ ssh = 2222 }}
#
# [[entities]]
# type = \"storage\"
# name = \"{PLACEHOLDER}-storage-1\"
#
# # storage を提供しているのがどの host か
# [[dependencies]]
# from = \"storage/{PLACEHOLDER}-storage-1\"
# to   = \"host/{PLACEHOLDER}-fileserver-1\"
# type = \"provides\"
#
# # その storage を使っている node
# [[dependencies]]
# from = \"host/{PLACEHOLDER}-compute-1\"
# to   = \"storage/{PLACEHOLDER}-storage-1\"
# type = \"uses_storage\"

# ===========================================================================
# capability の上書き（必要な場合のみ）
#
# 優先順位: disable > force > runtime discovery > enable / role hint
#
# 想定と違う場合は、まず対象 host で `sentinel doctor` を実行し、
# 判定理由を確認してください。
# ===========================================================================
# [capabilities]
# \"storage.nfs.server\" = \"force\"
"
    )
}

fn agent_capabilities_section() -> String {
    String::from(
        "\n\
# ===========================================================================
# capability の上書き（必要な場合のみ）
#
# capability は agent が自動検出するため、通常は何も書く必要がありません。
# `sentinel doctor` で判定理由を確認できます。
#
# この host を peer observer にする場合は下の 2 行を有効にしてください。
# observer は互いに異なる障害ドメインから選ぶこと。同じ storage の背後にいる
# 3 台は、視点 1 つを 3 回数えているだけです。
# ===========================================================================
# [capabilities]
# \"observer.peer\" = \"force\"
",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(role: Role) -> Config {
        Config::from_toml(&config_file(role), Path::new("generated.toml")).expect("generated config parses")
    }

    #[test]
    fn a_generated_file_parses() {
        parse(Role::Controller);
        parse(Role::Agent);
    }

    #[test]
    fn a_generated_file_passes_config_check() {
        // The whole point is that an operator can generate, edit one line and
        // start. A generated file that fails validation would make that false.
        for role in [Role::Controller, Role::Agent] {
            let report = crate::config::validate(&parse(role));
            let errors: Vec<_> = report.errors().collect();
            assert!(errors.is_empty(), "{:?}: {errors:?}", role.as_str());
        }
    }

    #[test]
    fn what_is_written_out_matches_the_compiled_defaults() {
        // The file claims to show defaults. If it drifts from Config::default
        // it is documentation that lies, which is worse than none.
        let generated = parse(Role::Controller);
        let defaults = Config::default();
        assert_eq!(generated.controller, defaults.controller);
        assert_eq!(generated.retention, defaults.retention);
        assert_eq!(generated.peer_monitoring, defaults.peer_monitoring);
        assert_eq!(generated.database, defaults.database);
    }

    #[test]
    fn every_probe_appears_with_its_real_schedule() {
        let text = config_file(Role::Agent);
        for entry in catalog::catalog() {
            assert!(
                text.contains(&format!("[probes.\"{}\"]", entry.id())),
                "{} is missing from the generated file",
                entry.id()
            );
            assert!(
                text.contains(&format!("# interval = \"{}\"", duration(entry.definition.interval))),
                "{} shows a schedule it does not have",
                entry.id()
            );
        }
    }

    #[test]
    fn the_probe_block_is_commented_out_so_defaults_stay_authoritative() {
        // Written-out probe overrides would freeze today's cadences into every
        // deployed file, and a later change to a default would never reach the
        // hosts that need it.
        let config = parse(Role::Agent);
        assert!(config.probes.is_empty());
    }

    #[test]
    fn only_the_lines_that_must_change_are_marked() {
        let agent = config_file(Role::Agent);
        // environment and controller_address, and nothing else uncommented.
        let live: Vec<&str> = agent
            .lines()
            .filter(|l| l.contains(PLACEHOLDER) && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(live.len(), 2, "{live:?}");
        assert!(live.iter().any(|l| l.starts_with("environment")));
        assert!(live.iter().any(|l| l.starts_with("controller_address")));
    }

    #[test]
    fn the_controller_file_needs_only_the_environment_changed() {
        let text = config_file(Role::Controller);
        let live: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(PLACEHOLDER) && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(live.len(), 1, "{live:?}");
        assert!(live[0].starts_with("environment"));
    }

    #[test]
    fn every_duration_written_out_parses_back_to_itself() {
        // A generated file that cannot be read back is not a configuration
        // file, it is a description of one.
        let generated = parse(Role::Controller);
        let defaults = Config::default();
        assert_eq!(
            generated.controller.inventory_interval,
            defaults.controller.inventory_interval
        );
        assert_eq!(generated.retention.transitions, defaults.retention.transitions);
        assert_eq!(generated.retention.observations, defaults.retention.observations);
    }

    #[test]
    fn an_agent_file_does_not_configure_the_controllers_database() {
        // Settings that do nothing on this host are settings someone will
        // change and then wonder why nothing happened.
        let text = config_file(Role::Agent);
        assert!(!text.contains("[retention]"), "{text}");
        assert!(!text.contains("[database]"));
    }

    #[test]
    fn a_role_round_trips() {
        for role in [Role::Controller, Role::Agent] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("compute"), None);
    }
}
