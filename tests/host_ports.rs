use ghis::config::{Rule, RuleContext, RuleResolution, resolve_rule};
use ghis::repo::parse_remote;

#[test]
fn enterprise_remote_port_participates_in_host_rule_resolution() {
    let remote = parse_remote("origin", "https://git.company.test:8443/acme/project.git");
    let rules = vec![Rule {
        id: "company-ghes".into(),
        profile: "work".into(),
        priority: 100,
        host: Some("git.company.test:8443".into()),
        ..Rule::default()
    }];
    let context = RuleContext {
        host: remote.host,
        owner: remote.owner,
        repo: remote.repo,
        remote: Some(remote.url),
        gitdir: None,
        cwd: None,
    };

    assert_eq!(
        resolve_rule(&rules, &context),
        RuleResolution::Match {
            rule_id: "company-ghes".into(),
            profile: "work".into(),
            priority: 100,
        }
    );
}

#[test]
fn explicit_https_443_uses_the_canonical_host_rule() {
    let remote = parse_remote("origin", "https://Git.Company.Test.:443/acme/project.git");
    let rules = vec![Rule {
        id: "company-ghes".into(),
        profile: "work".into(),
        priority: 100,
        host: Some("Git.Company.Test.:443".into()),
        ..Rule::default()
    }];
    let context = RuleContext {
        host: remote.host,
        ..RuleContext::default()
    };

    assert!(matches!(
        resolve_rule(&rules, &context),
        RuleResolution::Match { profile, .. } if profile == "work"
    ));
}
