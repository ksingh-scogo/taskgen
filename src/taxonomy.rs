use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand::Rng;
use rand::distributions::{Distribution, WeightedIndex};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: &str = "scogo.taskgen.taxonomy.v3";
const WEIGHT_TOLERANCE: f64 = 0.000_001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaxonomyKind {
    Hierarchical,
    Compositional,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TaskCoordinates {
    pub taxonomy_id: String,
    pub category_id: String,
    pub task_family: String,
    pub environment: String,
    pub platform_scope: String,
    pub platforms: Vec<String>,
    pub incident_mechanism: String,
    pub evidence_condition: String,
    pub evidence_bundle: String,
    pub action_risk: String,
    pub presentation: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct SampledTask {
    pub taxonomy_id: String,
    pub category_id: String,
    pub domain_id: String,
    pub domain_label: String,
    pub subdomain_id: String,
    pub coordinates: Option<TaskCoordinates>,
    pub difficulty: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct RawTaxonomy {
    schema_version: String,
    id: String,
    kind: String,
    label: String,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    categories: Vec<RawCategory>,
    #[serde(default)]
    platform_groups: Vec<RawPlatformGroup>,
    axes: Option<RawAxes>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDefaults {
    system_prompt_file: Option<PathBuf>,
    review_system_prompt_file: Option<PathBuf>,
    #[serde(default)]
    difficulty_distribution: HashMap<u8, f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCategory {
    id: String,
    label: String,
    weight: f64,
    #[serde(default)]
    eligibility: RawEligibility,
    #[serde(default)]
    domains: Vec<RawDomain>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDomain {
    id: String,
    label: String,
    weight: Option<f64>,
    #[serde(default)]
    eligibility: RawEligibility,
    #[serde(default)]
    subdomains: Vec<RawSubdomain>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawEligibility {
    task_families: Option<Vec<String>>,
    environments: Option<Vec<String>>,
    platform_scopes: Option<Vec<String>>,
    platform_groups: Option<Vec<String>>,
    platforms: Option<Vec<String>>,
    incident_mechanisms: Option<Vec<String>>,
    evidence_conditions: Option<Vec<String>>,
    evidence_bundles: Option<Vec<String>>,
    action_risks: Option<Vec<String>>,
    presentations: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEligibility {
    pub task_families: Vec<String>,
    pub environments: Vec<String>,
    pub platform_scopes: Vec<String>,
    pub platform_groups: Vec<String>,
    pub platforms: Vec<String>,
    pub incident_mechanisms: Vec<String>,
    pub evidence_conditions: Vec<String>,
    pub evidence_bundles: Vec<String>,
    pub action_risks: Vec<String>,
    pub presentations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubdomain {
    id: String,
    label: Option<String>,
    weight: Option<f64>,
    #[serde(default)]
    capabilities: Box<RawEligibility>,
}

impl RawSubdomain {
    fn id(&self) -> &str {
        &self.id
    }

    fn weight(&self) -> f64 {
        self.weight.unwrap_or(1.0)
    }

    fn capabilities(&self) -> &RawEligibility {
        &self.capabilities
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawAxes {
    #[serde(default)]
    task_families: Vec<RawWeightedOption>,
    #[serde(default)]
    environments: Vec<RawWeightedOption>,
    #[serde(default)]
    platform_scopes: Vec<RawWeightedOption>,
    #[serde(default)]
    incident_mechanisms: Vec<RawWeightedOption>,
    #[serde(default)]
    evidence_conditions: Vec<RawWeightedOption>,
    #[serde(default)]
    evidence_bundles: Vec<RawWeightedOption>,
    #[serde(default)]
    action_risks: Vec<RawWeightedOption>,
    #[serde(default)]
    presentations: Vec<RawWeightedOption>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawWeightedOption {
    id: String,
    #[serde(default)]
    label: String,
    weight: f64,
    #[serde(default = "default_true")]
    enabled: bool,
    difficulty_min: Option<u8>,
    difficulty_max: Option<u8>,
    #[serde(default)]
    difficulty_multiplier: HashMap<u8, f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPlatformGroup {
    id: String,
    weight: f64,
    #[serde(default)]
    platforms: Vec<RawPlatform>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPlatform {
    id: String,
    #[serde(default)]
    label: String,
    weight: f64,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct TaxonomyCatalog {
    source_path: Option<PathBuf>,
    raw: RawTaxonomy,
    kind: TaxonomyKind,
}

impl TaxonomyCatalog {
    pub fn from_yaml(source: &str, source_path: Option<&Path>) -> Result<Self> {
        let header: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(source).context("invalid taxonomy YAML")?;
        let schema_version = header
            .get("schema_version")
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or("<missing>");
        if schema_version != SCHEMA_VERSION {
            bail!("unsupported taxonomy schema '{schema_version}'; expected {SCHEMA_VERSION}");
        }
        let raw: RawTaxonomy = serde_yaml_ng::from_str(source).context("invalid taxonomy YAML")?;
        let kind = match raw.kind.as_str() {
            "compositional" => TaxonomyKind::Compositional,
            other => bail!(
                "unsupported taxonomy kind '{other}'; {SCHEMA_VERSION} requires 'compositional'"
            ),
        };
        let catalog = Self {
            source_path: source_path.map(Path::to_path_buf),
            raw,
            kind,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read taxonomy: {}", path.display()))?;
        Self::from_yaml(&source, Some(path))
            .with_context(|| format!("invalid taxonomy: {}", path.display()))
    }

    pub fn embedded_itops() -> Result<Self> {
        Self::from_yaml(include_str!("../docs/it-ops-taxonomy.yaml"), None)
    }

    pub fn validate(&self) -> Result<()> {
        if self.raw.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported taxonomy schema '{}'; expected {SCHEMA_VERSION}",
                self.raw.schema_version
            );
        }
        validate_nonempty_id("taxonomy", &self.raw.id)?;
        if self.raw.label.trim().is_empty() {
            bail!("taxonomy label must not be empty");
        }
        validate_difficulty(&self.raw.defaults.difficulty_distribution)?;

        self.validate_compositional()
    }

    fn validate_compositional(&self) -> Result<()> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("compositional taxonomy requires axes")?;
        if self.raw.categories.is_empty() {
            bail!("compositional taxonomy requires categories");
        }
        validate_unique_ids(
            "category",
            self.raw.categories.iter().map(|item| item.id.as_str()),
        )?;
        validate_complete_weights(
            "category distribution",
            self.raw
                .categories
                .iter()
                .map(|item| (item.id.as_str(), item.weight)),
        )?;
        let option_axes = [
            ("task_families", &axes.task_families),
            ("environments", &axes.environments),
            ("platform_scopes", &axes.platform_scopes),
            ("incident_mechanisms", &axes.incident_mechanisms),
            ("evidence_conditions", &axes.evidence_conditions),
            ("evidence_bundles", &axes.evidence_bundles),
            ("action_risks", &axes.action_risks),
            ("presentations", &axes.presentations),
        ];
        for (name, options) in option_axes {
            validate_options(name, options)?;
        }
        validate_unique_ids(
            "platform group",
            self.raw.platform_groups.iter().map(|item| item.id.as_str()),
        )?;
        if self.raw.platform_groups.is_empty() {
            bail!("compositional taxonomy requires platform_groups");
        }
        for group in &self.raw.platform_groups {
            validate_finite_weight(
                &format!("platform group '{}': weight", group.id),
                group.weight,
            )?;
            if group.platforms.is_empty() {
                bail!("platform group '{}' must contain platforms", group.id);
            }
            validate_unique_ids(
                &format!("platform in group '{}'", group.id),
                group.platforms.iter().map(|item| item.id.as_str()),
            )?;
            for platform in &group.platforms {
                if platform.label.trim().is_empty() {
                    bail!(
                        "platform '{}' in group '{}' label must not be empty",
                        platform.id,
                        group.id
                    );
                }
            }
            validate_complete_weights(
                &format!("platform distribution in group '{}'", group.id),
                group
                    .platforms
                    .iter()
                    .map(|item| (item.id.as_str(), item.weight)),
            )?;
        }

        for category in &self.raw.categories {
            self.validate_category(category)?;
        }
        Ok(())
    }

    fn validate_category(&self, category: &RawCategory) -> Result<()> {
        validate_nonempty_id("category", &category.id)?;
        if category.label.trim().is_empty() {
            bail!("category '{}' label must not be empty", category.id);
        }
        if category.domains.is_empty() {
            bail!(
                "category '{}' must contain at least one domain",
                category.id
            );
        }
        validate_unique_ids(
            &format!("domain in category '{}'", category.id),
            category.domains.iter().map(|domain| domain.id.as_str()),
        )?;
        let weighted = category
            .domains
            .iter()
            .filter(|domain| domain.weight.is_some())
            .count();
        if weighted != 0 && weighted != category.domains.len() {
            bail!(
                "category '{}' cannot mix weighted and unweighted domains",
                category.id
            );
        }
        if weighted == category.domains.len() {
            validate_complete_weights(
                &format!("domain distribution in category '{}'", category.id),
                category
                    .domains
                    .iter()
                    .map(|domain| (domain.id.as_str(), domain.weight.unwrap_or_default())),
            )?;
        }
        for domain in &category.domains {
            if domain.label.trim().is_empty() {
                bail!("domain '{}' label must not be empty", domain.id);
            }
            validate_subdomains(&category.id, &domain.id, &domain.subdomains)?;
            for subdomain in &domain.subdomains {
                let resolved = self.resolve_eligibility(category, domain, Some(subdomain))?;
                self.validate_platform_capacity(category, domain, subdomain, &resolved)?;
                self.validate_sampling_reachability(category, domain, subdomain, &resolved)?;
            }
        }
        Ok(())
    }

    fn validate_sampling_reachability(
        &self,
        category: &RawCategory,
        domain: &RawDomain,
        subdomain: &RawSubdomain,
        eligibility: &ResolvedEligibility,
    ) -> Result<()> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("missing compositional axes")?;
        let subject = format!("{}/{}/{}", category.id, domain.id, subdomain.id());
        for (name, options, eligible) in [
            (
                "task_families",
                &axes.task_families,
                &eligibility.task_families,
            ),
            (
                "environments",
                &axes.environments,
                &eligibility.environments,
            ),
            (
                "platform_scopes",
                &axes.platform_scopes,
                &eligibility.platform_scopes,
            ),
            (
                "incident_mechanisms",
                &axes.incident_mechanisms,
                &eligibility.incident_mechanisms,
            ),
            (
                "evidence_conditions",
                &axes.evidence_conditions,
                &eligibility.evidence_conditions,
            ),
            (
                "evidence_bundles",
                &axes.evidence_bundles,
                &eligibility.evidence_bundles,
            ),
            (
                "action_risks",
                &axes.action_risks,
                &eligibility.action_risks,
            ),
            (
                "presentations",
                &axes.presentations,
                &eligibility.presentations,
            ),
        ] {
            validate_eligible_axis_weight(&subject, name, options, eligible)?;
        }

        for scope in &eligibility.platform_scopes {
            if !matches!(
                scope.as_str(),
                "platform_neutral" | "single_platform" | "multi_platform"
            ) {
                bail!("subject '{subject}' uses unsupported platform scope '{scope}'");
            }
        }

        for family in axes.task_families.iter().filter(|family| {
            family.enabled && family.weight > 0.0 && eligibility.task_families.contains(&family.id)
        }) {
            let min = family.difficulty_min.unwrap_or(1);
            let max = family.difficulty_max.unwrap_or(10);
            let reachable_weight: f64 = self
                .raw
                .defaults
                .difficulty_distribution
                .iter()
                .filter(|(level, _)| **level >= min && **level <= max)
                .map(|(level, weight)| {
                    weight
                        * family
                            .difficulty_multiplier
                            .get(level)
                            .copied()
                            .unwrap_or(1.0)
                })
                .sum();
            if reachable_weight <= 0.0 {
                bail!(
                    "subject '{subject}' task family '{}' has no reachable positive-weight difficulty",
                    family.id
                );
            }
        }

        let positive_platforms = self.positive_weight_platform_count(eligibility);
        for scope in axes.platform_scopes.iter().filter(|scope| {
            scope.enabled && scope.weight > 0.0 && eligibility.platform_scopes.contains(&scope.id)
        }) {
            let required = match scope.id.as_str() {
                "platform_neutral" => 0,
                "single_platform" => 1,
                "multi_platform" => 2,
                _ => continue,
            };
            if positive_platforms < required {
                bail!(
                    "subject '{subject}' cannot sample '{}' from positive-weight platforms",
                    scope.id
                );
            }
            if scope.id == "platform_neutral" {
                let neutral_presentations: Vec<String> = eligibility
                    .presentations
                    .iter()
                    .filter(|presentation| presentation.as_str() != "cli_ssh_session")
                    .cloned()
                    .collect();
                validate_eligible_axis_weight(
                    &subject,
                    "platform-neutral presentations",
                    &axes.presentations,
                    &neutral_presentations,
                )?;
            }
        }
        Ok(())
    }

    fn positive_weight_platform_count(&self, eligibility: &ResolvedEligibility) -> usize {
        let mut weights: HashMap<&str, f64> = HashMap::new();
        for group in &self.raw.platform_groups {
            if !eligibility.platform_groups.contains(&group.id) || group.weight <= 0.0 {
                continue;
            }
            for platform in &group.platforms {
                if eligibility.platforms.contains(&platform.id) {
                    *weights.entry(platform.id.as_str()).or_default() +=
                        group.weight * platform.weight;
                }
            }
        }
        weights.values().filter(|weight| **weight > 0.0).count()
    }

    fn resolve_eligibility(
        &self,
        category: &RawCategory,
        domain: &RawDomain,
        subdomain: Option<&RawSubdomain>,
    ) -> Result<ResolvedEligibility> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("missing compositional axes")?;
        let platform_groups: Vec<String> = self
            .raw
            .platform_groups
            .iter()
            .map(|group| group.id.clone())
            .collect();
        let subdomain_capabilities = subdomain.map(RawSubdomain::capabilities);
        let resolved_platform_groups = resolve_axis(
            "platform_groups",
            subdomain_capabilities.and_then(|value| value.platform_groups.as_ref()),
            domain.eligibility.platform_groups.as_ref(),
            category.eligibility.platform_groups.as_ref(),
            &platform_groups,
        )?;
        let mut group_platforms = Vec::new();
        let mut all_platforms = Vec::new();
        for group in &self.raw.platform_groups {
            for platform in &group.platforms {
                if !all_platforms.contains(&platform.id) {
                    all_platforms.push(platform.id.clone());
                }
                if resolved_platform_groups.contains(&group.id)
                    && !group_platforms.contains(&platform.id)
                {
                    group_platforms.push(platform.id.clone());
                }
            }
        }
        let configured_platforms =
            subdomain_capabilities.and_then(|value| value.platforms.as_ref());
        let resolved_platforms = if configured_platforms.is_some()
            || domain.eligibility.platforms.is_some()
            || category.eligibility.platforms.is_some()
        {
            let resolved = resolve_axis(
                "platforms",
                configured_platforms,
                domain.eligibility.platforms.as_ref(),
                category.eligibility.platforms.as_ref(),
                &all_platforms,
            )?;
            let allowed: HashSet<&str> = group_platforms.iter().map(String::as_str).collect();
            validate_references("platforms allowed by platform_groups", &resolved, &allowed)?;
            resolved
        } else {
            group_platforms
        };
        Ok(ResolvedEligibility {
            task_families: resolve_axis(
                "task_families",
                subdomain_capabilities.and_then(|value| value.task_families.as_ref()),
                domain.eligibility.task_families.as_ref(),
                category.eligibility.task_families.as_ref(),
                &enabled_owned_ids(&axes.task_families),
            )?,
            environments: resolve_axis(
                "environments",
                subdomain_capabilities.and_then(|value| value.environments.as_ref()),
                domain.eligibility.environments.as_ref(),
                category.eligibility.environments.as_ref(),
                &enabled_owned_ids(&axes.environments),
            )?,
            platform_scopes: resolve_axis(
                "platform_scopes",
                subdomain_capabilities.and_then(|value| value.platform_scopes.as_ref()),
                domain.eligibility.platform_scopes.as_ref(),
                category.eligibility.platform_scopes.as_ref(),
                &enabled_owned_ids(&axes.platform_scopes),
            )?,
            platform_groups: resolved_platform_groups,
            platforms: resolved_platforms,
            incident_mechanisms: resolve_axis(
                "incident_mechanisms",
                subdomain_capabilities.and_then(|value| value.incident_mechanisms.as_ref()),
                domain.eligibility.incident_mechanisms.as_ref(),
                category.eligibility.incident_mechanisms.as_ref(),
                &enabled_owned_ids(&axes.incident_mechanisms),
            )?,
            evidence_conditions: resolve_axis(
                "evidence_conditions",
                subdomain_capabilities.and_then(|value| value.evidence_conditions.as_ref()),
                domain.eligibility.evidence_conditions.as_ref(),
                category.eligibility.evidence_conditions.as_ref(),
                &enabled_owned_ids(&axes.evidence_conditions),
            )?,
            evidence_bundles: resolve_axis(
                "evidence_bundles",
                subdomain_capabilities.and_then(|value| value.evidence_bundles.as_ref()),
                domain.eligibility.evidence_bundles.as_ref(),
                category.eligibility.evidence_bundles.as_ref(),
                &enabled_owned_ids(&axes.evidence_bundles),
            )?,
            action_risks: resolve_axis(
                "action_risks",
                subdomain_capabilities.and_then(|value| value.action_risks.as_ref()),
                domain.eligibility.action_risks.as_ref(),
                category.eligibility.action_risks.as_ref(),
                &enabled_owned_ids(&axes.action_risks),
            )?,
            presentations: resolve_axis(
                "presentations",
                subdomain_capabilities.and_then(|value| value.presentations.as_ref()),
                domain.eligibility.presentations.as_ref(),
                category.eligibility.presentations.as_ref(),
                &enabled_owned_ids(&axes.presentations),
            )?,
        })
    }

    fn validate_platform_capacity(
        &self,
        category: &RawCategory,
        domain: &RawDomain,
        subdomain: &RawSubdomain,
        eligibility: &ResolvedEligibility,
    ) -> Result<()> {
        let distinct: HashSet<&str> = eligibility.platforms.iter().map(String::as_str).collect();
        if eligibility
            .platform_scopes
            .iter()
            .any(|scope| scope == "single_platform")
            && distinct.is_empty()
        {
            bail!(
                "subdomain '{}/{}/{}' cannot satisfy platform scope 'single_platform'",
                category.id,
                domain.id,
                subdomain.id()
            );
        }
        if eligibility
            .platform_scopes
            .iter()
            .any(|scope| scope == "multi_platform")
            && distinct.len() < 2
        {
            bail!(
                "subdomain '{}/{}/{}' cannot satisfy platform scope 'multi_platform'",
                category.id,
                domain.id,
                subdomain.id()
            );
        }
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.raw.id
    }

    pub fn kind(&self) -> TaxonomyKind {
        self.kind
    }

    pub fn default_system_prompt_path(&self) -> Option<PathBuf> {
        self.resolve_prompt_path(self.raw.defaults.system_prompt_file.as_ref()?)
    }

    pub fn default_review_system_prompt_path(&self) -> Option<PathBuf> {
        self.resolve_prompt_path(self.raw.defaults.review_system_prompt_file.as_ref()?)
    }

    fn resolve_prompt_path(&self, configured: &Path) -> Option<PathBuf> {
        if configured.is_absolute() {
            return Some(configured.to_path_buf());
        }
        self.source_path
            .as_deref()
            .and_then(Path::parent)
            .map(|parent| parent.join(configured))
    }

    pub fn available_distribution_ids(&self) -> Vec<&str> {
        self.raw
            .categories
            .iter()
            .map(|item| item.id.as_str())
            .collect()
    }

    pub fn default_distribution(&self) -> HashMap<String, f64> {
        self.raw
            .categories
            .iter()
            .map(|item| (item.id.clone(), item.weight))
            .collect()
    }

    pub fn default_difficulty(&self) -> HashMap<u8, f64> {
        self.raw.defaults.difficulty_distribution.clone()
    }

    pub fn domain_count(&self) -> usize {
        self.raw
            .categories
            .iter()
            .map(|category| category.domains.len())
            .sum()
    }

    pub fn category_count(&self) -> usize {
        self.raw.categories.len()
    }

    pub fn platform_group_count(&self) -> usize {
        self.raw.platform_groups.len()
    }

    pub fn contains_hierarchical_subdomain(
        &self,
        category_id: &str,
        domain_name: &str,
        subdomain_id: &str,
    ) -> bool {
        self.raw.categories.iter().any(|category| {
            category.id == category_id
                && category.domains.iter().any(|domain| {
                    (domain.id == domain_name || domain.label == domain_name)
                        && domain
                            .subdomains
                            .iter()
                            .any(|subdomain| subdomain.id() == subdomain_id)
                })
        })
    }

    pub fn hierarchical_domain_count(&self, category_id: &str) -> usize {
        self.raw
            .categories
            .iter()
            .find(|category| category.id == category_id)
            .map(|category| category.domains.len())
            .unwrap_or(0)
    }

    pub fn subdomain_count(&self) -> usize {
        self.raw
            .categories
            .iter()
            .flat_map(|category| &category.domains)
            .map(|domain| domain.subdomains.len())
            .sum()
    }

    pub fn default_domain_weight_sum(&self) -> f64 {
        self.default_distribution().values().sum()
    }

    pub fn resolved_eligibility(
        &self,
        category_id: &str,
        domain_id: &str,
    ) -> Result<ResolvedEligibility> {
        let category = self
            .raw
            .categories
            .iter()
            .find(|category| category.id == category_id)
            .with_context(|| format!("unknown category '{category_id}'"))?;
        let domain = category
            .domains
            .iter()
            .find(|domain| domain.id == domain_id)
            .with_context(|| format!("unknown domain '{category_id}/{domain_id}'"))?;
        self.resolve_eligibility(category, domain, None)
    }

    pub fn resolved_subdomain_eligibility(
        &self,
        category_id: &str,
        domain_id: &str,
        subdomain_id: &str,
    ) -> Result<ResolvedEligibility> {
        let category = self
            .raw
            .categories
            .iter()
            .find(|category| category.id == category_id)
            .with_context(|| format!("unknown category '{category_id}'"))?;
        let domain = category
            .domains
            .iter()
            .find(|domain| domain.id == domain_id)
            .with_context(|| format!("unknown domain '{category_id}/{domain_id}'"))?;
        let subdomain = domain
            .subdomains
            .iter()
            .find(|subdomain| subdomain.id() == subdomain_id)
            .with_context(|| {
                format!("unknown subdomain '{category_id}/{domain_id}/{subdomain_id}'")
            })?;
        self.resolve_eligibility(category, domain, Some(subdomain))
    }

    pub fn validate_task_coordinates(
        &self,
        category_id: &str,
        domain_id: &str,
        subdomain_id: &str,
        coordinates: &TaskCoordinates,
    ) -> Result<()> {
        if coordinates.taxonomy_id != self.raw.id {
            bail!(
                "coordinate taxonomy_id '{}' does not match '{}'",
                coordinates.taxonomy_id,
                self.raw.id
            );
        }
        if coordinates.category_id != category_id {
            bail!(
                "coordinate category_id '{}' does not match task category '{}'",
                coordinates.category_id,
                category_id
            );
        }
        let eligibility =
            self.resolved_subdomain_eligibility(category_id, domain_id, subdomain_id)?;
        for (axis, value, allowed) in [
            (
                "task_family",
                &coordinates.task_family,
                &eligibility.task_families,
            ),
            (
                "environment",
                &coordinates.environment,
                &eligibility.environments,
            ),
            (
                "platform_scope",
                &coordinates.platform_scope,
                &eligibility.platform_scopes,
            ),
            (
                "incident_mechanism",
                &coordinates.incident_mechanism,
                &eligibility.incident_mechanisms,
            ),
            (
                "evidence_condition",
                &coordinates.evidence_condition,
                &eligibility.evidence_conditions,
            ),
            (
                "evidence_bundle",
                &coordinates.evidence_bundle,
                &eligibility.evidence_bundles,
            ),
            (
                "action_risk",
                &coordinates.action_risk,
                &eligibility.action_risks,
            ),
            (
                "presentation",
                &coordinates.presentation,
                &eligibility.presentations,
            ),
        ] {
            if !allowed.contains(value) {
                bail!(
                    "coordinate {axis} '{}' is not allowed for {category_id}/{domain_id}/{subdomain_id}",
                    value
                );
            }
        }
        if coordinates
            .platforms
            .iter()
            .any(|platform| !eligibility.platforms.contains(platform))
        {
            bail!(
                "coordinate platforms are not allowed for {category_id}/{domain_id}/{subdomain_id}"
            );
        }
        match coordinates.platform_scope.as_str() {
            "platform_neutral" if !coordinates.platforms.is_empty() => {
                bail!("platform_neutral coordinates must not select platforms")
            }
            "single_platform" if coordinates.platforms.len() != 1 => {
                bail!("single_platform coordinates must select exactly one platform")
            }
            "multi_platform" if coordinates.platforms.len() < 2 => {
                bail!("multi_platform coordinates must select at least two platforms")
            }
            _ => {}
        }
        Ok(())
    }

    pub fn sample_defaults<R: Rng + ?Sized>(&self, rng: &mut R) -> Result<SampledTask> {
        self.sample_compositional(
            rng,
            &self.default_distribution(),
            &self.default_difficulty(),
        )
    }

    pub fn sample<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        self.validate_sampling_distributions(distribution, difficulty)?;
        self.sample_compositional(rng, distribution, difficulty)
    }

    pub fn sample_prevalidated<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        self.sample_compositional(rng, distribution, difficulty)
    }

    pub fn validate_sampling_distributions(
        &self,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<()> {
        validate_override_distribution(self, distribution)?;
        validate_difficulty(difficulty)?;
        self.validate_effective_difficulty(distribution, difficulty)
    }

    fn validate_effective_difficulty(
        &self,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<()> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("missing compositional axes")?;
        for category in self
            .raw
            .categories
            .iter()
            .filter(|category| distribution[&category.id] > 0.0)
        {
            for domain in category
                .domains
                .iter()
                .filter(|domain| domain.weight.unwrap_or(1.0) > 0.0)
            {
                for subdomain in domain
                    .subdomains
                    .iter()
                    .filter(|subdomain| subdomain.weight() > 0.0)
                {
                    let eligibility =
                        self.resolve_eligibility(category, domain, Some(subdomain))?;
                    for family in axes.task_families.iter().filter(|family| {
                        family.enabled
                            && family.weight > 0.0
                            && eligibility.task_families.contains(&family.id)
                    }) {
                        let min = family.difficulty_min.unwrap_or(1);
                        let max = family.difficulty_max.unwrap_or(10);
                        let reachable_weight: f64 = difficulty
                            .iter()
                            .filter(|(level, _)| **level >= min && **level <= max)
                            .map(|(level, weight)| {
                                weight
                                    * family
                                        .difficulty_multiplier
                                        .get(level)
                                        .copied()
                                        .unwrap_or(1.0)
                            })
                            .sum();
                        if reachable_weight <= 0.0 {
                            bail!(
                                "subject '{}/{}/{}' task family '{}' has no reachable positive-weight difficulty in the effective distribution",
                                category.id,
                                domain.id,
                                subdomain.id(),
                                family.id
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn sample_compositional<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        let category_weights: Vec<f64> = self
            .raw
            .categories
            .iter()
            .map(|item| distribution[&item.id])
            .collect();
        let category =
            &self.raw.categories[sample_index(rng, &category_weights, "category distribution")?];
        let domain_weights: Vec<f64> = if category.domains[0].weight.is_some() {
            category
                .domains
                .iter()
                .map(|domain| domain.weight.unwrap_or_default())
                .collect()
        } else {
            vec![1.0; category.domains.len()]
        };
        let domain = &category.domains[sample_index(rng, &domain_weights, "domain distribution")?];
        let sub_weights: Vec<f64> = domain.subdomains.iter().map(RawSubdomain::weight).collect();
        let subdomain =
            &domain.subdomains[sample_index(rng, &sub_weights, "subdomain distribution")?];

        self.sample_subject_coordinates(rng, category, domain, subdomain, difficulty)
    }

    pub fn resample_subject_coordinates<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        subject: &SampledTask,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        validate_difficulty(difficulty)?;
        if subject.taxonomy_id != self.raw.id {
            bail!(
                "cannot recycle subject from taxonomy '{}' with '{}'",
                subject.taxonomy_id,
                self.raw.id
            );
        }
        let category = self
            .raw
            .categories
            .iter()
            .find(|category| category.id == subject.category_id)
            .with_context(|| format!("unknown category '{}'", subject.category_id))?;
        let domain = category
            .domains
            .iter()
            .find(|domain| domain.id == subject.domain_id)
            .with_context(|| {
                format!(
                    "unknown domain '{}/{}'",
                    subject.category_id, subject.domain_id
                )
            })?;
        let subdomain = domain
            .subdomains
            .iter()
            .find(|subdomain| subdomain.id() == subject.subdomain_id)
            .with_context(|| {
                format!(
                    "unknown subdomain '{}/{}/{}'",
                    subject.category_id, subject.domain_id, subject.subdomain_id
                )
            })?;
        self.sample_subject_coordinates(rng, category, domain, subdomain, difficulty)
    }

    pub fn resample_unseen_subject_coordinates<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        subject: &SampledTask,
        difficulty: &HashMap<u8, f64>,
        attempted: &HashSet<SampledTask>,
    ) -> Result<SampledTask> {
        const MAX_RESAMPLE_DRAWS: usize = 64;

        for _ in 0..MAX_RESAMPLE_DRAWS {
            let candidate = self.resample_subject_coordinates(rng, subject, difficulty)?;
            if !attempted.contains(&candidate) {
                return Ok(candidate);
            }
        }
        bail!(
            "no unseen coordinate composition found for subject '{}/{}/{}' after {MAX_RESAMPLE_DRAWS} draws",
            subject.category_id,
            subject.domain_id,
            subject.subdomain_id
        )
    }

    fn sample_subject_coordinates<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        category: &RawCategory,
        domain: &RawDomain,
        subdomain: &RawSubdomain,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("missing compositional axes")?;

        let eligibility = self.resolve_eligibility(category, domain, Some(subdomain))?;

        let task_family = sample_option(
            rng,
            &axes.task_families,
            Some(&eligibility.task_families),
            "task family",
        )?;
        let environment = sample_option(
            rng,
            &axes.environments,
            Some(&eligibility.environments),
            "environment",
        )?;
        let platform_scope = sample_option(
            rng,
            &axes.platform_scopes,
            Some(&eligibility.platform_scopes),
            "platform scope",
        )?;
        let platforms = self.sample_platforms(rng, &eligibility, &platform_scope.id)?;
        let mechanism = sample_option(
            rng,
            &axes.incident_mechanisms,
            Some(&eligibility.incident_mechanisms),
            "incident mechanism",
        )?;
        let evidence_condition = sample_option(
            rng,
            &axes.evidence_conditions,
            Some(&eligibility.evidence_conditions),
            "evidence condition",
        )?;
        let evidence_bundle = sample_option(
            rng,
            &axes.evidence_bundles,
            Some(&eligibility.evidence_bundles),
            "evidence bundle",
        )?;
        let action_risk = sample_option(
            rng,
            &axes.action_risks,
            Some(&eligibility.action_risks),
            "action risk",
        )?;
        let min = task_family.difficulty_min.unwrap_or(1);
        let max = task_family.difficulty_max.unwrap_or(10);
        let selected_difficulty = sample_difficulty(
            rng,
            difficulty,
            min,
            max,
            &task_family.difficulty_multiplier,
        )?;
        let presentation_eligibility: Vec<String> = if platform_scope.id == "platform_neutral" {
            eligibility
                .presentations
                .iter()
                .filter(|presentation| presentation.as_str() != "cli_ssh_session")
                .cloned()
                .collect()
        } else {
            eligibility.presentations.clone()
        };
        if presentation_eligibility.is_empty() {
            bail!(
                "platform-neutral sampling requires at least one non-CLI presentation for category '{}' domain '{}'",
                category.id,
                domain.id
            );
        }
        let presentation = sample_option(
            rng,
            &axes.presentations,
            Some(&presentation_eligibility),
            "presentation",
        )?;

        Ok(SampledTask {
            taxonomy_id: self.raw.id.clone(),
            category_id: category.id.clone(),
            domain_id: domain.id.clone(),
            domain_label: domain.label.clone(),
            subdomain_id: subdomain.id().to_string(),
            coordinates: Some(TaskCoordinates {
                taxonomy_id: self.raw.id.clone(),
                category_id: category.id.clone(),
                task_family: task_family.id.clone(),
                environment: environment.id.clone(),
                platform_scope: platform_scope.id.clone(),
                platforms,
                incident_mechanism: mechanism.id.clone(),
                evidence_condition: evidence_condition.id.clone(),
                evidence_bundle: evidence_bundle.id.clone(),
                action_risk: action_risk.id.clone(),
                presentation: presentation.id.clone(),
            }),
            difficulty: selected_difficulty,
        })
    }

    fn sample_platforms<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        eligibility: &ResolvedEligibility,
        scope: &str,
    ) -> Result<Vec<String>> {
        let count = match scope {
            "platform_neutral" => return Ok(Vec::new()),
            "single_platform" => 1,
            "multi_platform" => 2,
            other => bail!("unsupported platform scope '{other}'"),
        };
        let mut platforms = Vec::new();
        let mut weights = Vec::new();
        for group in &self.raw.platform_groups {
            if !eligibility.platform_groups.contains(&group.id) {
                continue;
            }
            for platform in &group.platforms {
                if !eligibility.platforms.contains(&platform.id) {
                    continue;
                }
                if let Some(existing) = platforms.iter().position(|id: &String| id == &platform.id)
                {
                    weights[existing] += group.weight * platform.weight;
                } else {
                    platforms.push(platform.id.clone());
                    weights.push(group.weight * platform.weight);
                }
            }
        }
        if platforms.len() < count {
            bail!("eligible platform groups cannot satisfy platform scope '{scope}'");
        }
        let mut selected = Vec::new();
        for _ in 0..count {
            let index = sample_index(rng, &weights, "platform distribution")?;
            selected.push(platforms.remove(index));
            weights.remove(index);
        }
        Ok(selected)
    }
}

fn validate_nonempty_id(kind: &str, id: &str) -> Result<()> {
    if id.trim().is_empty() {
        bail!("{kind} id must not be empty");
    }
    Ok(())
}

fn validate_unique_ids<'a>(kind: &str, ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        validate_nonempty_id(kind, id)?;
        if !seen.insert(id) {
            bail!("duplicate {kind} id '{id}'");
        }
    }
    Ok(())
}

fn validate_finite_weight(name: &str, weight: f64) -> Result<()> {
    if !weight.is_finite() || weight < 0.0 {
        bail!("{name} must be finite and non-negative, got {weight}");
    }
    Ok(())
}

fn validate_complete_weights<'a>(
    name: &str,
    weights: impl Iterator<Item = (&'a str, f64)>,
) -> Result<()> {
    let mut total = 0.0;
    let mut count = 0;
    for (id, weight) in weights {
        validate_finite_weight(&format!("{name} '{id}'"), weight)?;
        total += weight;
        count += 1;
    }
    if count == 0 {
        bail!("{name} must not be empty");
    }
    if (total - 1.0).abs() > WEIGHT_TOLERANCE {
        bail!("{name} weights must sum to 1.0, got {total}");
    }
    Ok(())
}

fn validate_difficulty(weights: &HashMap<u8, f64>) -> Result<()> {
    for level in weights.keys() {
        if !(1..=10).contains(level) {
            bail!("difficulty level must be 1-10, got {level}");
        }
    }
    validate_complete_weights(
        "difficulty distribution",
        weights
            .iter()
            .map(|(level, weight)| ("difficulty", *weight + (*level as f64 * 0.0))),
    )
}

fn validate_subdomains(category: &str, domain: &str, subdomains: &[RawSubdomain]) -> Result<()> {
    if subdomains.is_empty() {
        bail!("domain '{domain}' in '{category}' must contain subdomains");
    }
    validate_unique_ids(
        &format!("subdomain in '{category}/{domain}'"),
        subdomains.iter().map(RawSubdomain::id),
    )?;
    let mut total_weight = 0.0;
    for subdomain in subdomains {
        validate_finite_weight(
            &format!("subdomain '{category}/{domain}/{}' weight", subdomain.id()),
            subdomain.weight(),
        )?;
        total_weight += subdomain.weight();
        if subdomain
            .label
            .as_ref()
            .is_some_and(|label| label.trim().is_empty())
        {
            bail!(
                "subdomain '{category}/{domain}/{}' label must not be empty",
                subdomain.id()
            );
        }
    }
    if total_weight <= 0.0 {
        bail!("domain '{category}/{domain}' must have a positive subdomain weight");
    }
    Ok(())
}

fn validate_options(name: &str, options: &[RawWeightedOption]) -> Result<()> {
    let enabled: Vec<&RawWeightedOption> = options.iter().filter(|item| item.enabled).collect();
    validate_unique_ids(name, options.iter().map(|item| item.id.as_str()))?;
    validate_complete_weights(
        name,
        enabled.iter().map(|item| (item.id.as_str(), item.weight)),
    )?;
    for option in options {
        if option.label.trim().is_empty() {
            bail!("{name} option '{}' label must not be empty", option.id);
        }
        if let Some(min) = option.difficulty_min
            && !(1..=10).contains(&min)
        {
            bail!("{name} option '{}' difficulty_min must be 1-10", option.id);
        }
        if let Some(max) = option.difficulty_max
            && !(1..=10).contains(&max)
        {
            bail!("{name} option '{}' difficulty_max must be 1-10", option.id);
        }
        if option.difficulty_min.unwrap_or(1) > option.difficulty_max.unwrap_or(10) {
            bail!(
                "{name} option '{}' has inverted difficulty bounds",
                option.id
            );
        }
        for (level, multiplier) in &option.difficulty_multiplier {
            if !(1..=10).contains(level) {
                bail!(
                    "{name} option '{}' has invalid difficulty multiplier level {level}",
                    option.id
                );
            }
            validate_finite_weight(
                &format!("{name} option '{}' difficulty multiplier", option.id),
                *multiplier,
            )?;
        }
    }
    Ok(())
}

fn validate_eligible_axis_weight(
    subject: &str,
    name: &str,
    options: &[RawWeightedOption],
    eligible: &[String],
) -> Result<()> {
    let total: f64 = options
        .iter()
        .filter(|option| option.enabled && eligible.contains(&option.id))
        .map(|option| option.weight)
        .sum();
    if total <= 0.0 {
        bail!("subject '{subject}' has no positive-weight option for '{name}'");
    }
    Ok(())
}

fn enabled_owned_ids(options: &[RawWeightedOption]) -> Vec<String> {
    options
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.id.clone())
        .collect()
}

fn resolve_axis(
    name: &str,
    subdomain: Option<&Vec<String>>,
    domain: Option<&Vec<String>>,
    category: Option<&Vec<String>>,
    all_enabled: &[String],
) -> Result<Vec<String>> {
    let valid: HashSet<&str> = all_enabled.iter().map(String::as_str).collect();
    let mut selected = all_enabled.to_vec();
    for (level, configured) in [
        ("category", category),
        ("domain", domain),
        ("subdomain capabilities", subdomain),
    ] {
        let Some(configured) = configured else {
            continue;
        };
        validate_references(name, configured, &valid)?;
        let inherited: HashSet<&str> = selected.iter().map(String::as_str).collect();
        for value in configured {
            if !inherited.contains(value.as_str()) {
                bail!("{level} '{name}' capability '{value}' is outside inherited eligibility");
            }
        }
        selected = configured.clone();
    }
    if selected.is_empty() {
        bail!("resolved eligibility for '{name}' must not be empty");
    }
    Ok(selected)
}

fn validate_references(name: &str, references: &[String], valid: &HashSet<&str>) -> Result<()> {
    let mut seen = HashSet::new();
    for reference in references {
        if !seen.insert(reference) {
            bail!("{name} contains duplicate reference '{reference}'");
        }
        if !valid.contains(reference.as_str()) {
            bail!("{name} references unknown id '{reference}'");
        }
    }
    Ok(())
}

fn validate_override_distribution(
    catalog: &TaxonomyCatalog,
    distribution: &HashMap<String, f64>,
) -> Result<()> {
    let valid: HashSet<&str> = catalog.available_distribution_ids().into_iter().collect();
    for id in distribution.keys() {
        if !valid.contains(id.as_str()) {
            bail!("distribution references unknown id '{id}'");
        }
    }
    if distribution.len() != valid.len() || valid.iter().any(|id| !distribution.contains_key(*id)) {
        bail!("distribution must include every category exactly once");
    }
    validate_complete_weights(
        "distribution",
        distribution
            .iter()
            .map(|(id, weight)| (id.as_str(), *weight)),
    )
}

fn sample_index<R: Rng + ?Sized>(rng: &mut R, weights: &[f64], name: &str) -> Result<usize> {
    let distribution =
        WeightedIndex::new(weights).with_context(|| format!("invalid or empty {name}"))?;
    Ok(distribution.sample(rng))
}

fn sample_option<'a, R: Rng + ?Sized>(
    rng: &mut R,
    options: &'a [RawWeightedOption],
    allow: Option<&[String]>,
    name: &str,
) -> Result<&'a RawWeightedOption> {
    let eligible: Vec<&RawWeightedOption> = options
        .iter()
        .filter(|item| item.enabled)
        .filter(|item| allow.is_none_or(|allowed| allowed.contains(&item.id)))
        .collect();
    let weights: Vec<f64> = eligible.iter().map(|item| item.weight).collect();
    let index = sample_index(rng, &weights, name)?;
    Ok(eligible[index])
}

fn sample_difficulty<R: Rng + ?Sized>(
    rng: &mut R,
    distribution: &HashMap<u8, f64>,
    min: u8,
    max: u8,
    multipliers: &HashMap<u8, f64>,
) -> Result<u8> {
    let mut levels: Vec<u8> = distribution
        .keys()
        .copied()
        .filter(|level| *level >= min && *level <= max)
        .collect();
    levels.sort_unstable();
    let weights: Vec<f64> = levels
        .iter()
        .map(|level| distribution[level] * multipliers.get(level).copied().unwrap_or(1.0))
        .collect();
    let index = sample_index(rng, &weights, "difficulty distribution")?;
    Ok(levels[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    const V3_FIXTURE: &str = r#"
schema_version: scogo.taskgen.taxonomy.v3
id: compositional-test
kind: compositional
label: Compositional Test
defaults:
  difficulty_distribution: {1: 1.0}
platform_groups:
  - id: network
    weight: 1.0
    platforms:
      - {id: platform_a, label: Platform A, weight: 0.5}
      - {id: platform_b, label: Platform B, weight: 0.5}
axes:
  task_families:
    - {id: investigate, label: Investigate, weight: 1.0, difficulty_min: 1, difficulty_max: 10}
  environments:
    - {id: on_premises, label: On premises, weight: 0.5}
    - {id: hybrid, label: Hybrid, weight: 0.5}
  platform_scopes:
    - {id: platform_neutral, label: Platform neutral, weight: 0.34}
    - {id: single_platform, label: Single platform, weight: 0.33}
    - {id: multi_platform, label: Multi platform, weight: 0.33}
  incident_mechanisms:
    - {id: misconfiguration, label: Misconfiguration, weight: 1.0}
  evidence_conditions:
    - {id: partial, label: Partial, weight: 1.0}
  evidence_bundles:
    - {id: configuration_state, label: Configuration state, weight: 1.0}
  action_risks:
    - {id: read_only_investigation, label: Read only, weight: 1.0}
  presentations:
    - {id: service_desk_ticket, label: Service desk ticket, weight: 1.0}
categories:
  - id: network
    label: Network
    weight: 1.0
    eligibility:
      environments: [on_premises, hybrid]
      platform_groups: [network]
    domains:
      - id: routing
        label: Routing
        subdomains:
          - {id: bgp, capabilities: {}}
          - {id: ospf, capabilities: {}}
        eligibility:
          environments: [hybrid]
"#;

    #[test]
    fn rejects_v1_taxonomy_after_compositional_unification() {
        let v1 = r#"
schema_version: scogo.taskgen.taxonomy.v1
id: old
kind: hierarchical
label: Old
defaults:
  difficulty_distribution: {1: 1.0}
categories:
  - id: only
    label: Only
    weight: 1.0
    domains:
      - name: Domain
        subdomains: [failure]
"#;
        let error = TaxonomyCatalog::from_yaml(v1, None).unwrap_err();
        assert!(
            error.to_string().contains("scogo.taskgen.taxonomy.v3"),
            "{error:#}"
        );
    }

    #[test]
    fn rejects_compositions_that_cannot_be_sampled() {
        let zero_weight_environment = V3_FIXTURE
            .replace(
                "    - {id: on_premises, label: On premises, weight: 0.5}\n    - {id: hybrid, label: Hybrid, weight: 0.5}",
                "    - {id: on_premises, label: On premises, weight: 1.0}\n    - {id: hybrid, label: Hybrid, weight: 0.0}",
            );
        assert!(TaxonomyCatalog::from_yaml(&zero_weight_environment, None).is_err());

        let unsupported_scope = V3_FIXTURE.replace(
            "{id: platform_neutral, label: Platform neutral, weight: 0.34}",
            "{id: unsupported_scope, label: Unsupported, weight: 0.34}",
        );
        assert!(TaxonomyCatalog::from_yaml(&unsupported_scope, None).is_err());

        let unreachable_difficulty = V3_FIXTURE.replace(
            "difficulty_min: 1, difficulty_max: 10",
            "difficulty_min: 2, difficulty_max: 10",
        );
        assert!(TaxonomyCatalog::from_yaml(&unreachable_difficulty, None).is_err());
    }

    #[test]
    fn parses_nested_v3_taxonomy_and_samples_universal_coordinates() {
        let catalog = TaxonomyCatalog::from_yaml(V3_FIXTURE, None).unwrap();
        assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
        assert_eq!(catalog.category_count(), 1);
        assert_eq!(catalog.domain_count(), 1);
        assert_eq!(catalog.subdomain_count(), 2);

        let sample = catalog
            .sample_defaults(&mut StdRng::seed_from_u64(7))
            .unwrap();
        let coordinates = sample.coordinates.expect("coordinates");
        assert_eq!(coordinates.category_id, sample.category_id);
        assert_eq!(coordinates.environment, "hybrid");
        assert!(matches!(
            coordinates.platform_scope.as_str(),
            "platform_neutral" | "single_platform" | "multi_platform"
        ));
    }

    #[test]
    fn subdomain_eligibility_controls_scope_and_exact_platforms() {
        let yaml = V3_FIXTURE.replace(
            "        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
            "        subdomains:\n          - id: bgp\n            capabilities:\n              platform_scopes: [single_platform]\n              platforms: [platform_b]",
        );
        let catalog = TaxonomyCatalog::from_yaml(&yaml, None).unwrap();
        let mut rng = StdRng::seed_from_u64(17);

        for _ in 0..32 {
            let sample = catalog.sample_defaults(&mut rng).unwrap();
            let coordinates = sample.coordinates.unwrap();
            assert_eq!(coordinates.platform_scope, "single_platform");
            assert_eq!(coordinates.platforms, vec!["platform_b"]);
        }
    }

    #[test]
    fn rejects_subdomain_platform_outside_eligible_groups() {
        let yaml = V3_FIXTURE
            .replace(
                "      - {id: platform_b, label: Platform B, weight: 0.5}",
                "      - {id: platform_b, label: Platform B, weight: 0.5}\n  - id: other\n    weight: 1.0\n    platforms:\n      - {id: platform_c, label: Platform C, weight: 1.0}",
            )
            .replace(
                "        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
                "        subdomains:\n          - id: bgp\n            capabilities:\n              platform_scopes: [single_platform]\n              platforms: [platform_c]",
            );

        let error = TaxonomyCatalog::from_yaml(&yaml, None).unwrap_err();
        assert!(
            error.to_string().contains(
                "platforms allowed by platform_groups references unknown id 'platform_c'"
            ),
            "{error:#}"
        );
    }

    #[test]
    fn rejects_mixed_weighted_and_unweighted_domains() {
        let invalid = V3_FIXTURE.replacen(
            "      - id: routing\n        label: Routing\n        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
            "      - id: routing\n        label: Routing\n        weight: 0.5\n        subdomains: [{id: bgp, capabilities: {}}]\n      - id: switching\n        label: Switching\n        subdomains: [{id: vlan, capabilities: {}}]",
            1,
        );
        let error = TaxonomyCatalog::from_yaml(&invalid, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("mix weighted and unweighted domains"),
            "{error:#}"
        );
    }

    #[test]
    fn loads_and_validates_embedded_itops_taxonomy() {
        let catalog =
            TaxonomyCatalog::from_yaml(include_str!("../docs/it-ops-taxonomy.yaml"), None)
                .expect("embedded IT Ops taxonomy should parse");

        assert_eq!(catalog.id(), "scogo-itops-v4");
        assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
        assert_eq!(catalog.category_count(), 14);
        assert_eq!(catalog.domain_count(), 129);
        assert_eq!(catalog.subdomain_count(), 884);
        catalog
            .validate()
            .expect("embedded taxonomy should validate");
    }

    #[test]
    fn itops_v4_migration_report_is_lossless() {
        let source = fs::read_to_string("docs/it-ops-taxonomy-v4-migration.json")
            .expect("checked-in migration report");
        let report: serde_json::Value = serde_json::from_str(&source).unwrap();
        assert_eq!(report["source_counts"]["categories"], 14);
        assert_eq!(report["source_counts"]["domains"], 129);
        assert_eq!(report["source_counts"]["subdomains"], 884);
        assert_eq!(report["source_counts"], report["target_counts"]);
        assert_eq!(report["missing_domains"], serde_json::json!([]));
        assert_eq!(report["missing_subdomains"], serde_json::json!([]));
        assert_eq!(report["duplicate_target_ids"], serde_json::json!([]));
    }

    #[test]
    fn rejects_distribution_that_does_not_sum_to_one() {
        let yaml = V3_FIXTURE.replacen(
            "  - id: network\n    label: Network\n    weight: 1.0\n    eligibility:",
            "  - id: network\n    label: Network\n    weight: 0.9\n    eligibility:",
            1,
        );

        let error = TaxonomyCatalog::from_yaml(&yaml, None).unwrap_err();
        assert!(error.to_string().contains("sum to 1.0"), "{error:#}");
    }

    #[test]
    fn rejects_partial_category_override_before_sampling() {
        let catalog = TaxonomyCatalog::embedded_itops().unwrap();
        let first = catalog.available_distribution_ids()[0].to_string();
        let distribution = HashMap::from([(first, 1.0)]);

        let error = catalog
            .validate_sampling_distributions(&distribution, &catalog.default_difficulty())
            .unwrap_err();

        assert!(error.to_string().contains("must include every category"));
    }

    #[test]
    fn rejects_difficulty_override_unreachable_by_eligible_task_family() {
        let source = V3_FIXTURE
            .replace(
                "difficulty_distribution: {1: 1.0}",
                "difficulty_distribution: {2: 1.0}",
            )
            .replace(
                "difficulty_min: 1, difficulty_max: 10",
                "difficulty_min: 2, difficulty_max: 10",
            );
        let catalog = TaxonomyCatalog::from_yaml(&source, None).unwrap();
        let difficulty = HashMap::from([(1, 1.0)]);

        let error = catalog
            .validate_sampling_distributions(&catalog.default_distribution(), &difficulty)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no reachable positive-weight difficulty")
        );
    }

    #[test]
    fn rejects_domain_with_no_positive_subdomain_weight() {
        let yaml = V3_FIXTURE.replace(
            "        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
            "        subdomains:\n          - {id: disabled_a, weight: 0.0, capabilities: {}}\n          - {id: disabled_b, weight: 0.0, capabilities: {}}",
        );

        let error = TaxonomyCatalog::from_yaml(&yaml, None).unwrap_err();
        assert!(
            error.to_string().contains("positive subdomain weight"),
            "{error:#}"
        );
    }

    #[test]
    fn netops_taxonomy_has_exact_inventory_and_seeded_sampling() {
        let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
            .expect("checked-in NetOps taxonomy should parse");
        assert_eq!(catalog.id(), "scogo-enterprise-netops-v2");
        assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
        assert_eq!(catalog.category_count(), 1);
        assert_eq!(catalog.domain_count(), 25);
        assert_eq!(catalog.subdomain_count(), 531);
        assert_eq!(catalog.platform_group_count(), 15);
        assert!((catalog.default_domain_weight_sum() - 1.0).abs() < WEIGHT_TOLERANCE);

        let mut first = StdRng::seed_from_u64(42);
        let mut second = StdRng::seed_from_u64(42);
        assert_eq!(
            catalog.sample_defaults(&mut first).expect("first sample"),
            catalog.sample_defaults(&mut second).expect("second sample")
        );
    }

    #[test]
    fn netops_vendor_bound_subdomains_resolve_compatible_platforms() {
        let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();

        let aci = catalog
            .resolved_subdomain_eligibility(
                "enterprise_netops",
                "sdn_network_virtualization",
                "aci_controller",
            )
            .unwrap();
        assert_eq!(aci.platform_scopes, vec!["single_platform"]);
        assert_eq!(aci.platforms, vec!["cisco_aci"]);

        let prefix_delegation = catalog
            .resolved_subdomain_eligibility(
                "enterprise_netops",
                "container_kubernetes_networking",
                "cloud_cni_prefix_delegation",
            )
            .unwrap();
        assert_eq!(prefix_delegation.platform_scopes, vec!["single_platform"]);
        assert_eq!(prefix_delegation.platforms, vec!["aws_vpc_cni"]);
    }

    #[test]
    fn coordinate_recycling_preserves_subject_identity() {
        let catalog = TaxonomyCatalog::from_yaml(V3_FIXTURE, None).unwrap();
        let mut rng = StdRng::seed_from_u64(91);
        let initial = catalog.sample_defaults(&mut rng).unwrap();

        for _ in 0..32 {
            let replacement = catalog
                .resample_subject_coordinates(&mut rng, &initial, &catalog.default_difficulty())
                .unwrap();
            assert_eq!(replacement.category_id, initial.category_id);
            assert_eq!(replacement.domain_id, initial.domain_id);
            assert_eq!(replacement.subdomain_id, initial.subdomain_id);
        }
    }

    #[test]
    fn coordinate_recycling_fails_when_subject_has_no_unseen_composition() {
        let singleton = V3_FIXTURE
            .replace(
                "    - {id: platform_neutral, label: Platform neutral, weight: 0.34}\n    - {id: single_platform, label: Single platform, weight: 0.33}\n    - {id: multi_platform, label: Multi platform, weight: 0.33}",
                "    - {id: platform_neutral, label: Platform neutral, weight: 1.0}",
            )
            .replace(
                "        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
                "        subdomains: [{id: bgp, capabilities: {}}]",
            );
        let catalog = TaxonomyCatalog::from_yaml(&singleton, None).unwrap();
        let mut rng = StdRng::seed_from_u64(7);
        let initial = catalog.sample_defaults(&mut rng).unwrap();
        let attempted = HashSet::from([initial.clone()]);

        let error = catalog
            .resample_unseen_subject_coordinates(
                &mut rng,
                &initial,
                &catalog.default_difficulty(),
                &attempted,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no unseen coordinate composition")
        );
    }

    #[test]
    fn netops_prompts_preserve_scope_and_harness_boundary() {
        let taskgen = include_str!("../prompts/netops-taskgen-system-v2.txt");
        let teacher = include_str!("../prompts/netops-teacher-system-v1.txt");
        for excluded in [
            "3GPP",
            "EPC",
            "5GC",
            "IMS",
            "OSS/BSS",
            "service-provider core",
        ] {
            assert!(taskgen.contains(excluded), "missing exclusion {excluded}");
        }
        assert!(taskgen.contains("operational behavior, not certification recall"));
        assert!(taskgen.contains("internally and causally consistent"));
        assert!(taskgen.contains("scenario fixtures"));
        assert!(taskgen.contains("must not imply live access"));
        assert!(taskgen.contains("Never disguise pseudocode as captured production output"));
        assert!(taskgen.contains("Output only the final user task prompt."));
        assert!(teacher.contains("ATIF serialization"));
        assert!(teacher.contains("The harness, not you"));
    }

    #[test]
    fn both_generator_prompts_distinguish_scenario_fixtures_from_live_state() {
        for prompt in [
            include_str!("../prompts/netops-taskgen-system-v2.txt"),
            include_str!("../prompts/itops-taskgen-system-v2.txt"),
        ] {
            assert!(prompt.contains("scenario fixtures"));
            assert!(prompt.contains("must not imply live access"));
            assert!(prompt.contains("observations from hypotheses"));
        }
    }

    #[test]
    fn both_taxonomies_resolve_specific_review_prompts() {
        let itops = TaxonomyCatalog::from_path(Path::new("docs/it-ops-taxonomy.yaml")).unwrap();
        let netops = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();
        assert_eq!(
            itops.default_review_system_prompt_path().unwrap(),
            PathBuf::from("docs/../prompts/itops-prompt-review-system-v3.txt")
        );
        assert_eq!(
            netops.default_review_system_prompt_path().unwrap(),
            PathBuf::from("docs/../prompts/netops-prompt-review-system-v3.txt")
        );
    }

    #[test]
    fn platform_neutral_coordinates_never_request_device_cli_capture() {
        for catalog in [
            TaxonomyCatalog::embedded_itops().unwrap(),
            TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap(),
        ] {
            let mut rng = StdRng::seed_from_u64(20260820);
            for _ in 0..1000 {
                let sample = catalog.sample_defaults(&mut rng).unwrap();
                let coordinates = sample.coordinates.unwrap();
                assert!(
                    coordinates.platform_scope != "platform_neutral"
                        || coordinates.presentation != "cli_ssh_session",
                    "{} sampled platform-neutral CLI capture in {}/{}",
                    catalog.id(),
                    sample.domain_id,
                    sample.subdomain_id
                );
            }
        }
    }

    #[test]
    fn v3_rejects_bare_subdomain_ids() {
        let yaml = V3_FIXTURE.replace(
            "        subdomains:\n          - {id: bgp, capabilities: {}}\n          - {id: ospf, capabilities: {}}",
            "        subdomains: [bgp, ospf]",
        );
        let error = TaxonomyCatalog::from_yaml(&yaml, None).unwrap_err();
        assert!(format!("{error:#}").contains("subdomain"), "{error:#}");
    }

    #[test]
    fn netops_known_vendor_architecture_coordinates_are_compiled_out() {
        let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();

        let selectors = catalog
            .resolved_subdomain_eligibility(
                "enterprise_netops",
                "vpn_remote_access",
                "phase1_phase2_selectors",
            )
            .unwrap();
        assert!(!selectors.platforms.contains(&"sonic".to_string()));

        let virtual_systems = catalog
            .resolved_subdomain_eligibility(
                "enterprise_netops",
                "firewall_network_security",
                "vrf_vdom_vsys",
            )
            .unwrap();
        assert!(
            !virtual_systems
                .platforms
                .contains(&"google_cloud".to_string())
        );
        assert!(
            virtual_systems
                .platforms
                .contains(&"fortinet_fortios".to_string())
        );
    }

    #[test]
    fn coordinate_compiler_rejects_platform_outside_subdomain_capability() {
        let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();
        let mut rng = StdRng::seed_from_u64(91);
        let mut sample = catalog.sample_defaults(&mut rng).unwrap();
        sample.category_id = "enterprise_netops".into();
        sample.domain_id = "firewall_network_security".into();
        sample.subdomain_id = "vrf_vdom_vsys".into();
        let mut coordinates = sample.coordinates.unwrap();
        coordinates.category_id = "enterprise_netops".into();
        coordinates.platform_scope = "single_platform".into();
        coordinates.platforms = vec!["google_cloud".into()];

        let error = catalog
            .validate_task_coordinates(
                &sample.category_id,
                &sample.domain_id,
                &sample.subdomain_id,
                &coordinates,
            )
            .unwrap_err();
        assert!(error.to_string().contains("platforms are not allowed"));
    }
}
