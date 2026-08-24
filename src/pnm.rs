//! Finding the operator's VTA the way `pnm` itself finds it.
//!
//! Setup bootstraps from an existing `pnm` login, which means it has to agree
//! with `pnm` about three things this module owns:
//!
//! 1. **What identifies a VTA.** The canonical identifier is its **DID**. A
//!    slug is a nickname chosen on one machine at `pnm setup --name`, and means
//!    nothing anywhere else — so a setup instruction written in terms of a slug
//!    cannot be copied between machines or into a runbook. [`resolve`] therefore
//!    takes either, distinguished by the `did:` prefix, and falls back to
//!    `pnm`'s own `default_vta` when given neither.
//!
//! 2. **Where the session actually lives.** `pnm` stores sessions under the
//!    keyring key **`vta:<slug>`**, not `<slug>` — see `vta_keyring_key` in
//!    `pnm-cli/src/config.rs`. Neither session backend prefixes anything, so a
//!    bare slug simply finds no session and reports "Not authenticated" to
//!    somebody who is, in fact, authenticated. [`ResolvedVta::session_key`] is
//!    the one place that prefix is applied.
//!
//! 3. **Endpoints `pnm` was told about.** `VtaConfig` can carry an explicit
//!    `mediator_did` and `url` for VTAs that cannot advertise a service
//!    endpoint — a `did:key` VTA, or an airgapped one. Discovering the mediator
//!    from the DID document misses those, so they are read from here first and
//!    resolution is the fallback, not the other way round.
//!
//! This reads `~/.config/pnm/config.toml` directly because `pnm-cli` is a binary
//! crate with no library target — there is nothing to depend on. The shape read
//! is deliberately narrow (`default_vta`, and per-VTA `vta_did` / `url` /
//! `mediator_did`), and unknown keys are ignored, so `pnm` growing new fields
//! does not break this.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Deserialize;

/// One VTA target as `pnm` records it. Unknown fields are ignored on purpose:
/// this is a reader of somebody else's file, and it should not break when that
/// file gains a key.
#[derive(Debug, Clone, Deserialize)]
pub struct VtaTarget {
    /// Human-facing name.
    #[serde(default)]
    pub name: Option<String>,
    /// The VTA's DID. Absent while a `pnm setup` is still pending.
    #[serde(default)]
    pub vta_did: Option<String>,
    /// Explicit REST URL, for DIDs that cannot advertise a service endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// Explicit mediator DID, for `did:key` or airgapped VTAs.
    #[serde(default)]
    pub mediator_did: Option<String>,
}

/// The subset of `~/.config/pnm/config.toml` this tool reads.
#[derive(Debug, Default, Deserialize)]
pub struct PnmConfig {
    /// Slug used when no VTA is named.
    #[serde(default)]
    pub default_vta: Option<String>,
    /// Configured targets, keyed by slug.
    #[serde(default)]
    pub vtas: BTreeMap<String, VtaTarget>,
}

/// A VTA target resolved to everything setup needs to reach it.
#[derive(Debug, Clone)]
pub struct ResolvedVta {
    /// The `pnm` slug.
    pub slug: String,
    /// The VTA's DID — the identifier worth writing down.
    pub vta_did: String,
    /// Mediator DID `pnm` was configured with, if any.
    pub mediator_did: Option<String>,
    /// REST URL `pnm` was configured with, if any.
    pub url: Option<String>,
}

impl ResolvedVta {
    /// The keyring key the session is stored under: `vta:<slug>`.
    ///
    /// Passing the bare slug is the mistake this exists to prevent — it finds
    /// nothing and reports an authentication failure to someone who is
    /// authenticated.
    pub fn session_key(&self) -> String {
        format!("vta:{}", self.slug)
    }
}

impl PnmConfig {
    /// `$PNM_CONFIG_DIR`, else `~/.config/pnm`.
    pub fn default_dir() -> anyhow::Result<PathBuf> {
        if let Ok(d) = std::env::var("PNM_CONFIG_DIR")
            && !d.is_empty()
        {
            return Ok(PathBuf::from(d));
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| anyhow::anyhow!("HOME is not set; set PNM_CONFIG_DIR"))?;
        Ok(PathBuf::from(home).join(".config").join("pnm"))
    }

    /// Read `<dir>/config.toml`. A missing file is not an error — it is an
    /// operator who has not run `pnm setup` yet, and [`resolve`] says so far
    /// better than a file-not-found would.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("config.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Resolve a VTA DID, a slug, or nothing (meaning `pnm`'s default).
    ///
    /// Errors name the VTAs that *are* configured, with their DIDs, because the
    /// commonest failure here is a person guessing at a nickname they chose
    /// months ago on a different machine.
    pub fn resolve(&self, wanted: Option<&str>) -> anyhow::Result<ResolvedVta> {
        if self.vtas.is_empty() {
            bail!(
                "no VTA is configured on this machine.\n\nLog in first:\n  pnm setup --name <name>"
            );
        }

        let slug = match wanted {
            // A DID is the portable identifier, so it is matched first.
            Some(w) if w.starts_with("did:") => self
                .vtas
                .iter()
                .find(|(_, v)| v.vta_did.as_deref() == Some(w))
                .map(|(slug, _)| slug.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no `pnm` login for VTA `{w}` on this machine.\n\nConfigured:\n{}\n\nLog \
                         in to it with:\n  pnm setup --name <name>",
                        self.render_targets()
                    )
                })?,
            Some(w) => {
                if !self.vtas.contains_key(w) {
                    bail!(
                        "no `pnm` VTA named `{w}`.\n\nConfigured:\n{}",
                        self.render_targets()
                    );
                }
                w.to_string()
            }
            None => self.default_vta.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "no VTA named and `pnm` has no default.\n\nPass one:\n  --vta <did:… or \
                     name>\n\nConfigured:\n{}",
                    self.render_targets()
                )
            })?,
        };

        let target = self
            .vtas
            .get(&slug)
            .ok_or_else(|| anyhow::anyhow!("`pnm`'s default VTA `{slug}` is not configured"))?;

        // No DID means `pnm setup` was started and never finished. Saying that
        // beats an authentication error thirty seconds later.
        let vta_did = target.vta_did.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "the `pnm` VTA `{slug}` is pending setup — its admin DID was minted but the VTA \
                 DID has not been supplied.\n\nFinish it first:\n  pnm setup continue {slug}"
            )
        })?;

        Ok(ResolvedVta {
            slug,
            vta_did,
            mediator_did: target.mediator_did.clone(),
            url: target.url.clone(),
        })
    }

    /// Configured VTAs as `  name (slug) — did`, for error messages.
    fn render_targets(&self) -> String {
        if self.vtas.is_empty() {
            return "  (none)".to_string();
        }
        self.vtas
            .iter()
            .map(|(slug, v)| {
                let did = v.vta_did.as_deref().unwrap_or("(pending setup)");
                match v.name.as_deref() {
                    Some(n) if n != slug => format!("  {n} ({slug}) — {did}"),
                    _ => format!("  {slug} — {did}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml_src: &str) -> PnmConfig {
        toml::from_str(toml_src).expect("parses")
    }

    fn two_vtas() -> PnmConfig {
        cfg(r#"
default_vta = "personal"

[vtas.personal]
name = "Personal"
vta_did = "did:webvh:abc:example.com:personal"

[vtas.work]
name = "Work"
vta_did = "did:webvh:def:corp.example:work"
mediator_did = "did:web:mediator.corp.example"
url = "https://vta.corp.example"
"#)
    }

    #[test]
    fn the_session_key_carries_pnms_vta_prefix() {
        // The bug this whole module exists for: `pnm` stores under
        // `vta:<slug>`, and neither session backend prefixes anything, so a
        // bare slug reports "Not authenticated" to someone who is.
        let r = two_vtas().resolve(Some("personal")).unwrap();
        assert_eq!(r.session_key(), "vta:personal");
    }

    #[test]
    fn a_vta_did_resolves_to_its_slug() {
        let r = two_vtas()
            .resolve(Some("did:webvh:def:corp.example:work"))
            .unwrap();
        assert_eq!(r.slug, "work");
        assert_eq!(r.session_key(), "vta:work");
    }

    #[test]
    fn a_slug_still_works() {
        assert_eq!(two_vtas().resolve(Some("work")).unwrap().slug, "work");
    }

    #[test]
    fn nothing_named_uses_pnms_default() {
        let r = two_vtas().resolve(None).unwrap();
        assert_eq!(r.slug, "personal");
    }

    #[test]
    fn configured_endpoints_are_carried_through() {
        // `pnm` records these for VTAs that cannot advertise a service endpoint
        // (did:key, airgapped). Setup must prefer them over DID-doc discovery,
        // which by definition cannot find them.
        let r = two_vtas().resolve(Some("work")).unwrap();
        assert_eq!(
            r.mediator_did.as_deref(),
            Some("did:web:mediator.corp.example")
        );
        assert_eq!(r.url.as_deref(), Some("https://vta.corp.example"));
    }

    #[test]
    fn an_unknown_did_lists_what_is_configured() {
        let err = two_vtas().resolve(Some("did:webvh:zzz:nope")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Personal (personal)"), "{msg}");
        assert!(msg.contains("did:webvh:def:corp.example:work"), "{msg}");
    }

    #[test]
    fn an_unknown_slug_lists_what_is_configured() {
        let err = two_vtas().resolve(Some("typo")).unwrap_err();
        assert!(err.to_string().contains("Work (work)"), "{err}");
    }

    #[test]
    fn a_pending_setup_says_so_rather_than_failing_to_authenticate() {
        let c = cfg(r#"
default_vta = "half"
[vtas.half]
name = "Half done"
"#);
        let err = c.resolve(None).unwrap_err();
        assert!(err.to_string().contains("pnm setup continue half"), "{err}");
    }

    #[test]
    fn no_configured_vtas_points_at_pnm_setup() {
        let err = PnmConfig::default().resolve(None).unwrap_err();
        assert!(err.to_string().contains("pnm setup --name"), "{err}");
    }

    #[test]
    fn no_default_and_no_argument_asks_for_one() {
        let c = cfg(r#"
[vtas.only]
vta_did = "did:key:zVta"
"#);
        let err = c.resolve(None).unwrap_err();
        assert!(err.to_string().contains("--vta"), "{err}");
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = PnmConfig::load(dir.path()).unwrap();
        assert!(c.vtas.is_empty());
        // …but resolving against it explains what to do.
        assert!(
            c.resolve(None)
                .unwrap_err()
                .to_string()
                .contains("pnm setup")
        );
    }

    #[test]
    fn unknown_keys_in_pnms_config_are_ignored() {
        // This reads someone else's file; it must not break when that file
        // grows a field.
        let c = cfg(r#"
default_vta = "a"
resolver_url = "ws://127.0.0.1:4445/did/v1/ws"

[vtas.a]
name = "A"
vta_did = "did:key:zA"
some_future_key = "whatever"
"#);
        assert_eq!(c.resolve(None).unwrap().slug, "a");
    }
}
