use crate::assert_mem_size;
use crate::diagnostic::Severity;
use crate::errors::Errors;
use crate::path_helpers;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use toml::{Table, Value};

const DEFAULT_EXCLUDED_DIRECTORIES: &[&str] = &[
    ".bundle",
    ".claude",
    ".git",
    ".github",
    ".ruby-lsp",
    ".vscode",
    "log",
    "node_modules",
    "tmp",
];

/// The graph's settings, read from the `[graph]` section of the configuration file
#[derive(Debug, Clone)]
pub struct GraphSettings {
    /// Patterns to exclude from file discovery during indexing, on top of the built-in defaults. Stored as written and
    /// only joined with the workspace path when read, so that a pattern cannot outlive the configuration it came from
    /// and be silently re-rooted under another workspace.
    excluded_patterns: HashSet<Box<str>>,
}

impl Default for GraphSettings {
    /// The settings of a workspace that configures nothing: the built-in exclusions and no more
    fn default() -> Self {
        Self {
            excluded_patterns: DEFAULT_EXCLUDED_DIRECTORIES.iter().map(|&dir| Box::from(dir)).collect(),
        }
    }
}

impl GraphSettings {
    /// Parses the `[graph]` section on top of the default exclusions
    fn parse(mut table: Table) -> Result<Self, String> {
        let exclude = match table.remove("exclude") {
            None => Vec::new(),
            Some(value) => value
                .try_into::<Vec<Box<str>>>()
                .map_err(|error| format!("invalid `graph.exclude` setting: {error}"))?,
        };

        if let Some(key) = table.keys().next() {
            return Err(format!("unknown setting `graph.{key}`"));
        }

        let mut settings = Self::default();
        settings.exclude_patterns(exclude);
        Ok(settings)
    }

    /// Adds patterns to exclude from file discovery during indexing
    fn exclude_patterns(&mut self, patterns: impl IntoIterator<Item = Box<str>>) {
        self.excluded_patterns.extend(patterns);
    }

    /// Returns the exclusion patterns as written, without resolving them against any workspace
    fn excluded_patterns(&self) -> impl Iterator<Item = &str> {
        self.excluded_patterns.iter().map(|pattern| &**pattern)
    }
}

/// The setting of a single linter rule, read from a `[linter.rules.RuleName]` table
#[derive(Debug, Clone)]
pub struct Rule {
    name: Box<str>,
    enabled: bool,
    exclude_patterns: Box<[Box<str>]>,
    severity: Option<Severity>,
}

impl Rule {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn exclude_patterns(&self) -> &[Box<str>] {
        &self.exclude_patterns
    }

    #[must_use]
    pub fn severity(&self) -> Option<&Severity> {
        self.severity.as_ref()
    }

    /// Parses a single `[linter.rules.{name}]` table
    fn parse(name: &str, value: Value) -> Result<Self, String> {
        let Value::Table(mut table) = value else {
            return Err(format!("invalid `linter.rules.{name}` setting: expected a table"));
        };

        let enabled = match table.remove("enabled") {
            Some(Value::Boolean(enabled)) => enabled,
            Some(_) => {
                return Err(format!(
                    "invalid `linter.rules.{name}.enabled` setting: expected a boolean"
                ));
            }
            None => true,
        };

        let exclude_patterns = match table.remove("exclude") {
            Some(value) => value
                .try_into::<Vec<Box<str>>>()
                .map_err(|error| format!("invalid `linter.rules.{name}.exclude` setting: {error}"))?
                .into_boxed_slice(),
            None => Box::default(),
        };

        let severity = match table.remove("severity") {
            Some(value) => Some(
                value
                    .try_into::<Severity>()
                    .map_err(|error| format!("invalid `linter.rules.{name}.severity` setting: {error}"))?,
            ),
            None => None,
        };

        if let Some(key) = table.keys().next() {
            return Err(format!("unknown setting `linter.rules.{name}.{key}`"));
        }

        Ok(Self {
            name: Box::from(name),
            enabled,
            exclude_patterns,
            severity,
        })
    }
}

/// The linter's settings, read from the `[linter]` section of the configuration file
#[derive(Debug, Clone, Default)]
pub struct LinterSettings {
    rules: Box<[Rule]>,
}

impl LinterSettings {
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Parses the `[linter]` section
    fn parse(mut table: Table) -> Result<Self, String> {
        let rules = match table.remove("rules") {
            None => Box::default(),
            Some(Value::Table(rules)) => rules
                .into_iter()
                .map(|(name, value)| Rule::parse(&name, value))
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(String::from(
                    "invalid `linter.rules` setting: expected a table of rules",
                ));
            }
        };

        if let Some(key) = table.keys().next() {
            return Err(format!("unknown setting `linter.{key}`"));
        }

        Ok(Self { rules })
    }
}

/// The disk index's settings, read from the `[disk_index]` section of the configuration file
#[derive(Debug, Clone)]
pub struct DiskIndexSettings {
    /// Whether the disk-backed (low-resident-memory) index runs for this workspace. Defaults to
    /// false: the in-memory path stays the unprescriptive default, and a workspace opts in
    /// deliberately by committing the section.
    enabled: bool,
    /// Where the store lives: the literal `"tmp"` (the workspace's own `tmp/`, the Rails
    /// convention), the literal `"global"` (the platform cache directory), or an absolute
    /// directory. Empty means unconfigured, and the context-aware default decides (workspace
    /// `tmp/` when one exists, the platform cache directory otherwise).
    location: Box<str>,
}

impl Default for DiskIndexSettings {
    /// The settings of a workspace that configures nothing: disabled, with the location left to
    /// the context-aware default
    fn default() -> Self {
        Self {
            enabled: false,
            location: Box::from(""),
        }
    }
}

impl DiskIndexSettings {
    /// Parses the `[disk_index]` section
    fn parse(mut table: Table) -> Result<Self, String> {
        let enabled = match table.remove("enabled") {
            None => false,
            Some(value) => value
                .try_into::<bool>()
                .map_err(|error| format!("invalid `disk_index.enabled` setting: {error}"))?,
        };

        let location = match table.remove("location") {
            None => Box::from(""),
            Some(value) => value
                .try_into::<Box<str>>()
                .map_err(|error| format!("invalid `disk_index.location` setting: {error}"))?,
        };

        // A relative location is ambiguous: it would mean different things depending on the
        // process's working directory, which is not what the workspace configured it for.
        if !(location.is_empty()
            || location.as_ref() == "tmp"
            || location.as_ref() == "global"
            || Path::new(location.as_ref()).is_absolute())
        {
            return Err(format!(
                "invalid `disk_index.location` setting: must be `\"tmp\", `\"global\", or an absolute path, got `{location}`"
            ));
        }

        if let Some(key) = table.keys().next() {
            return Err(format!("unknown setting `disk_index.{key}`"));
        }

        Ok(DiskIndexSettings { enabled, location })
    }

    /// Whether the disk-backed index runs for this workspace
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Where the store lives as configured: `"tmp"`, `"global"`, an absolute directory, or empty
    /// when the workspace left it to the context-aware default
    #[must_use]
    pub fn location(&self) -> Box<str> {
        Box::from(self.location.as_ref())
    }
}

/// The configuration of a workspace, parsed from its `rubydex.toml` and shared by all built-in tools. It carries both
/// the settings that are global to every tool, such as the workspace being analyzed, and the typed settings of each
/// tool's own section (e.g. `[graph]`). Every section is parsed eagerly, so that all validation happens at load time and
/// unknown sections or settings are rejected. Load the file once and hand the configuration to each consumer, so that
/// none of them has to read it again.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory of the workspace being analyzed, which is where its configuration file lives. Global, and not
    /// configurable through the file itself, since it is what says where that file is.
    workspace_path: Box<Path>,
    graph: GraphSettings,
    linter: LinterSettings,
    disk_index: DiskIndexSettings,
}
assert_mem_size!(Config, 104);

impl Default for Config {
    /// The configuration of the current working directory, with the default settings of every section, which is what a
    /// graph is configured by until one is loaded for it. Guessing a root here rather than leaving it empty is what
    /// keeps `Graph::new` infallible, since patterns are resolved against it; every other configuration comes from
    /// [`Config::load`]. Cannot be derived, as `Box<Path>` has no default.
    fn default() -> Self {
        Self {
            workspace_path: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .into_boxed_path(),
            graph: GraphSettings::default(),
            linter: LinterSettings::default(),
            disk_index: DiskIndexSettings::default(),
        }
    }
}

impl Config {
    /// Loads the configuration of the workspace rooted at `workspace_path`, which is where its `rubydex.toml` is
    /// expected to be. The configuration file itself is optional: a workspace without one gets the default settings.
    ///
    /// # Errors
    ///
    /// Returns [`Errors::ConfigError`] if `workspace_path` is not a directory, or if the configuration file exists but
    /// cannot be read or is invalid.
    pub fn load(workspace_path: &Path) -> Result<Self, Errors> {
        let workspace_path = path_helpers::resolved(workspace_path).map_err(|error| {
            Errors::ConfigError(format!(
                "Failed to resolve workspace path `{}`: {error}",
                workspace_path.display()
            ))
        })?;

        // Since loading a configuration is how a workspace is chosen, a root that cannot be analyzed has to be rejected
        // here. Otherwise the mistake is only reported much later, by whatever first tries to walk it.
        if !workspace_path.is_dir() {
            return Err(Errors::ConfigError(format!(
                "Workspace `{}` is not a directory",
                workspace_path.display()
            )));
        }

        let config_path = workspace_path.join("rubydex.toml");

        let content = match fs::read_to_string(&config_path) {
            Ok(content) => content,
            // Configuring a workspace is optional, so a missing file is the same as an empty one
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(Errors::ConfigError(format!(
                    "Failed to read config file `{}`: {error}",
                    config_path.display()
                )));
            }
        };

        Self::parse(workspace_path, &content)
            .map_err(|error| Errors::ConfigError(format!("Invalid config file `{}`: {error}", config_path.display())))
    }

    /// Returns the root directory of the workspace this configuration belongs to
    #[must_use]
    pub fn workspace_path(&self) -> &Path {
        &self.workspace_path
    }

    /// Adds patterns to exclude from file discovery during indexing. Excluded directories will be skipped entirely
    /// during directory traversal.
    pub fn exclude_patterns(&mut self, patterns: impl IntoIterator<Item = Box<str>>) {
        self.graph.exclude_patterns(patterns);
    }

    /// Returns the set of exclusion patterns resolved against the workspace path. Resolving is the one thing that needs
    /// both halves of the configuration, which is why it lives here rather than on the settings of the section the
    /// patterns come from.
    #[must_use]
    pub fn excluded_patterns(&self) -> HashSet<Box<str>> {
        self.graph
            .excluded_patterns()
            .map(|pattern| {
                // We must replace the separator on Windows for forward slash to use in glob patterns.
                self.workspace_path
                    .join(pattern)
                    .to_string_lossy()
                    .replace(MAIN_SEPARATOR, "/")
                    .into_boxed_str()
            })
            .collect()
    }

    #[must_use]
    pub fn linter(&self) -> &LinterSettings {
        &self.linter
    }

    /// Returns the disk index's settings, which the Ruby side reads to decide both whether the
    /// disk-backed index runs and where its store lives
    #[must_use]
    pub fn disk_index(&self) -> &DiskIndexSettings {
        &self.disk_index
    }

    /// Parses the content of the configuration file of the workspace rooted at `workspace_path` into the typed
    /// settings of each section
    fn parse(workspace_path: PathBuf, content: &str) -> Result<Self, String> {
        let mut sections: Table = toml::from_str(content).map_err(|error| error.to_string())?;

        // Every top-level entry must be a section table. A non-table entry is most likely a typo, and an array of
        // tables comes from `[[section]]` syntax, which is a section shape mistake rather than an unknown setting.
        if let Some((key, value)) = sections.iter().find(|(_, value)| !value.is_table()) {
            if value
                .as_array()
                .is_some_and(|array| !array.is_empty() && array.iter().all(Value::is_table))
            {
                return Err(format!(
                    "section `{key}` must be a table; use `[{key}]` instead of `[[{key}]]`"
                ));
            }

            return Err(format!("unknown setting `{key}`"));
        }

        // Non-table entries were rejected above, so every present section is always a table.
        let graph = match sections.remove("graph") {
            Some(Value::Table(table)) => GraphSettings::parse(table)?,
            _ => GraphSettings::default(),
        };

        let linter = match sections.remove("linter") {
            Some(Value::Table(table)) => LinterSettings::parse(table)?,
            _ => LinterSettings::default(),
        };

        let disk_index = match sections.remove("disk_index") {
            Some(Value::Table(table)) => DiskIndexSettings::parse(table)?,
            _ => DiskIndexSettings::default(),
        };

        // Every section must be backed by a typed settings struct, so any leftover section is unknown.
        if let Some(key) = sections.keys().next() {
            return Err(format!("unknown section `{key}`"));
        }

        Ok(Self {
            workspace_path: Box::from(workspace_path),
            graph,
            linter,
            disk_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pattern the exclusion `entry` resolves to under `workspace_path`, spelled the way exclusions are
    fn exclusion(workspace_path: impl AsRef<Path>, entry: &str) -> String {
        workspace_path
            .as_ref()
            .join(entry)
            .to_string_lossy()
            .replace(MAIN_SEPARATOR, "/")
    }

    fn workspace_exclusion(entry: &str) -> String {
        exclusion("/workspace", entry)
    }

    /// Parses configuration content for an arbitrary workspace, for the tests that only care about the settings
    fn parse(content: &str) -> Result<Config, String> {
        Config::parse(PathBuf::from("/workspace"), content)
    }

    #[test]
    fn excluded_patterns_resolves_patterns_against_the_workspace_path() {
        let mut config = parse("").expect("an empty config is valid");
        config.exclude_patterns([
            Box::from("vendor"),
            Box::from("**/fixtures"),
            Box::from("/absolute/path"),
        ]);

        let excluded = config.excluded_patterns();

        let vendor = workspace_exclusion("vendor");
        let fixtures = workspace_exclusion("**/fixtures");
        let absolute = PathBuf::from("/absolute/path").to_string_lossy().into_owned();
        let git = workspace_exclusion(".git");

        assert!(excluded.contains(vendor.as_str()));
        assert!(excluded.contains(fixtures.as_str()));
        assert!(excluded.contains(absolute.as_str()));
        // Defaults are included and resolved as well.
        assert!(excluded.contains(git.as_str()));
    }

    #[test]
    fn excluded_patterns_are_separated_by_forward_slashes() {
        // Exclusions are glob patterns, and a glob pattern is written with forward slashes whatever the platform's own
        // separator is. Joining them against the workspace path is what would otherwise introduce a backslash, so the
        // invariant is asserted on the joined result. Vacuous where the separator already is a forward slash.
        let mut config = parse("").expect("an empty config is valid");
        config.exclude_patterns([Box::from("vendor/bundle")]);

        let excluded = config.excluded_patterns();

        assert!(!excluded.is_empty(), "expected the defaults at least");
        assert!(
            excluded.iter().all(|pattern| !pattern.contains('\\')),
            "unexpected backslash in {excluded:?}"
        );
    }

    #[test]
    fn load_parses_the_settings_of_every_section() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        fs::write(
            dir.path().join("rubydex.toml"),
            "[graph]\nexclude = [\"vendor\"]\n\n[linter.rules.Something]\nenabled = true\n",
        )
        .unwrap();

        let config = Config::load(dir.path()).expect("expected the config file to load");
        let excluded = config.excluded_patterns();

        let path = path_helpers::resolved(dir.path()).unwrap();
        assert!(excluded.contains(exclusion(&path, "vendor").as_str()));

        let rules = config.linter().rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name(), "Something");
        assert!(rules[0].enabled());
    }

    #[test]
    fn parse_parses_every_linter_rule() {
        let config = parse(
            "[linter.rules.Something]\nseverity = \"warning\"\nexclude = [\"components/legacy/**\"]\n\n\
             [linter.rules.Other]\nenabled = false\n",
        )
        .expect("expected the config to parse");

        let rules = config.linter().rules();
        assert_eq!(rules.len(), 2);

        let something = rules.iter().find(|rule| rule.name() == "Something").unwrap();
        assert!(something.enabled());
        assert_eq!(something.exclude_patterns(), [Box::from("components/legacy/**")]);
        assert_eq!(something.severity(), Some(&Severity::Warning));

        let other = rules.iter().find(|rule| rule.name() == "Other").unwrap();
        assert!(!other.enabled());
        assert_eq!(other.exclude_patterns(), []);
        assert_eq!(other.severity(), None);
    }

    #[test]
    fn parse_accepts_an_empty_linter_section() {
        let config = parse("[linter]\n").expect("an empty linter section is valid");
        assert!(config.linter().rules().is_empty());

        let config = parse("[linter.rules]\n").expect("an empty rules table is valid");
        assert!(config.linter().rules().is_empty());
    }

    #[test]
    fn parse_rejects_an_unknown_section() {
        let error = parse("[lintr]\n").expect_err("every section must be backed by typed settings structs");
        assert!(error.contains("unknown section `lintr`"), "unexpected error: {error}");
    }

    #[test]
    fn parse_rejects_an_unknown_linter_setting() {
        let error = parse("[linter]\nparallel = true\n").expect_err("typos inside the linter section must be rejected");

        assert!(
            error.contains("unknown setting `linter.parallel`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_a_rules_setting_that_is_not_a_table() {
        let error = parse("[linter]\nrules = true\n").expect_err("rules must be a table of rule tables");

        assert!(
            error.contains("invalid `linter.rules` setting"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_a_rule_that_is_not_a_table() {
        let error = parse("[linter.rules]\nSomething = true\n").expect_err("every rule setting must be a table");

        assert!(
            error.contains("invalid `linter.rules.Something` setting"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_defaults_a_rule_to_enabled() {
        let config = parse("[linter.rules.Something]\n").expect("enabled defaults to true");
        assert!(config.linter().rules()[0].enabled());
    }

    #[test]
    fn parse_rejects_a_non_boolean_enabled_setting() {
        let error = parse("[linter.rules.Something]\nenabled = \"yes\"\n").expect_err("enabled only accepts a boolean");

        assert!(
            error.contains("invalid `linter.rules.Something.enabled` setting"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_an_unknown_rule_setting() {
        let error =
            parse("[linter.rules.Something]\nparallel = true\n").expect_err("unknown rule settings must be rejected");

        assert!(
            error.contains("unknown setting `linter.rules.Something.parallel`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_an_invalid_rule_exclude_setting() {
        let error = parse("[linter.rules.Something]\nexclude = \"components/legacy/**\"\n")
            .expect_err("exclude must be an array of strings");

        assert!(
            error.contains("invalid `linter.rules.Something.exclude` setting"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_an_invalid_rule_severity_setting() {
        for severity in ["\"critical\"", "1"] {
            let error = parse(&format!("[linter.rules.Something]\nseverity = {severity}\n"))
                .expect_err("severity must be one of the supported strings");

            assert!(
                error.contains("invalid `linter.rules.Something.severity` setting"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn parse_accepts_every_rule_severity() {
        for (value, expected) in [
            ("error", Severity::Error),
            ("warning", Severity::Warning),
            ("information", Severity::Information),
            ("hint", Severity::Hint),
        ] {
            let config =
                parse(&format!("[linter.rules.Something]\nseverity = \"{value}\"\n")).expect("severity should parse");

            assert_eq!(config.linter().rules()[0].severity(), Some(&expected));
        }
    }

    #[test]
    fn load_returns_the_default_configuration_for_a_workspace_without_a_config_file() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let config = Config::load(dir.path()).expect("a missing rubydex.toml must not be an error");

        assert_eq!(config.workspace_path(), path_helpers::resolved(dir.path()).unwrap());
        assert_eq!(config.excluded_patterns().len(), DEFAULT_EXCLUDED_DIRECTORIES.len());
        assert!(config.linter().rules().is_empty());
    }

    #[test]
    fn load_reads_the_config_file_under_the_workspace_path() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        fs::write(dir.path().join("rubydex.toml"), "[graph]\nexclude = [\"vendor\"]\n").unwrap();

        let config = Config::load(dir.path()).expect("expected rubydex.toml to load");
        let excluded = config.excluded_patterns();

        let path = path_helpers::resolved(dir.path()).unwrap();
        assert_eq!(config.workspace_path(), path);
        assert!(excluded.contains(exclusion(&path, "vendor").as_str()));
        assert!(excluded.contains(exclusion(&path, ".git").as_str()));
    }

    #[test]
    fn load_errors_when_the_workspace_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let missing = dir.path().join("typo");
        let file = dir.path().join("file.rb");
        fs::write(&file, "class Foo; end").unwrap();

        for workspace_path in [missing.as_path(), file.as_path()] {
            let error = Config::load(workspace_path).expect_err("a workspace must be a directory");
            let named = path_helpers::resolved(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf());

            assert!(matches!(error, Errors::ConfigError(_)), "unexpected error: {error:?}");
            assert!(
                error.to_string().contains(&named.display().to_string()),
                "expected the error to name the workspace `{}`: {error}",
                named.display()
            );
        }
    }

    #[test]
    fn load_errors_when_the_config_file_cannot_be_read() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A `rubydex.toml` that exists but cannot be read is a broken workspace, unlike one that has no configuration
        // at all. Making it a directory is the portable way of making it unreadable.
        fs::create_dir(dir.path().join("rubydex.toml")).unwrap();

        let error = Config::load(dir.path()).expect_err("an unreadable config file must be an error");

        assert!(matches!(error, Errors::ConfigError(_)), "unexpected error: {error:?}");
        assert!(
            error.to_string().contains("Failed to read config file"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn load_propagates_malformed_config_errors() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        fs::write(dir.path().join("rubydex.toml"), "[graph]\nexclude = [\n").unwrap();

        let error = Config::load(dir.path()).expect_err("a malformed config file must be an error");

        assert!(matches!(error, Errors::ConfigError(_)), "unexpected error: {error:?}");
    }

    #[test]
    fn parse_accepts_an_empty_config() {
        let config = parse("").expect("an empty config is valid");

        // Nothing is configured, so the settings of every section are the default ones.
        assert_eq!(config.excluded_patterns().len(), DEFAULT_EXCLUDED_DIRECTORIES.len());
        assert!(config.linter().rules().is_empty());
    }

    #[test]
    fn parse_rejects_an_exclude_value_of_the_wrong_type() {
        let error =
            parse("[graph]\nexclude = \"vendor\"").expect_err("exclude must be an array of strings, not a string");

        assert!(error.contains("graph.exclude"), "unexpected error: {error}");
    }

    #[test]
    fn parse_rejects_an_unknown_top_level_setting() {
        let error = parse("excludes = [\"vendor\"]\n").expect_err("every top-level entry must be a tool section table");

        assert!(
            error.contains("unknown setting `excludes`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_an_unknown_graph_setting() {
        let error = parse("[graph]\nexcludes = [\"vendor\"]\n")
            .expect_err("the graph section is owned by Rubydex, so typos inside it must be rejected");

        assert!(
            error.contains("unknown setting `graph.excludes`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_rejects_a_graph_section_that_is_not_a_table() {
        let error = parse("graph = \"yes\"\n").expect_err("the graph section must be a table");

        assert!(error.contains("`graph`"), "unexpected error: {error}");
    }

    #[test]
    fn parse_rejects_an_array_of_tables_section() {
        let error =
            parse("[[linter]]\nparallel = true\n").expect_err("array-of-tables syntax is not a valid tool section");

        assert!(
            error.contains("use `[linter]` instead of `[[linter]]`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn disk_index_accepts_its_section() {
        let config = parse("[disk_index]\nenabled = true\nlocation = \"tmp\"\n").expect("a valid disk_index section");

        assert!(config.disk_index.enabled);
        assert_eq!(&*config.disk_index.location, "tmp");
    }

    #[test]
    fn disk_index_defaults_to_disabled_and_unconfigured() {
        let config = parse("").expect("an empty config is valid");

        assert!(!config.disk_index.enabled);
        assert!(config.disk_index.location.is_empty());
    }

    #[test]
    fn disk_index_accepts_an_absolute_location() {
        let config =
            parse("[disk_index]\nlocation = \"/var/tmp/indexes\"\n").expect("an absolute location is a valid choice");

        assert_eq!(&*config.disk_index.location, "/var/tmp/indexes");
    }

    #[test]
    fn disk_index_rejects_a_relative_location() {
        let error = parse("[disk_index]\nlocation = \"cache\"\n").expect_err("a relative location is ambiguous");

        assert!(error.contains("disk_index.location"), "unexpected error: {error}");
    }

    #[test]
    fn disk_index_rejects_a_non_boolean_enabled_setting() {
        let error = parse("[disk_index]\nenabled = \"yes\"\n").expect_err("enabled must be a boolean");

        assert!(error.contains("disk_index.enabled"), "unexpected error: {error}");
    }

    #[test]
    fn disk_index_rejects_an_unknown_setting() {
        let error = parse("[disk_index]\nmax_bytes = 1024\n").expect_err("unknown settings are typos");

        assert!(
            error.contains("unknown setting `disk_index.max_bytes`"),
            "unexpected error: {error}"
        );
    }
}
