use anyhow::{anyhow, Context as _, Result};
use clap::{
    parser::{MatchesError, ValueSource},
    Arg, ArgAction, ArgMatches, Command,
};
use serde::Deserialize;
use std::result;
use std::{collections::HashMap, iter, path::PathBuf, str::FromStr};
use toml::Table;

pub struct Config {
    args: ArgMatches,
    env_prefix: &'static str,
    env: HashMap<String, String>,
    files: Vec<(PathBuf, Table)>,
}

struct KeyNames {
    key: String,
    env_key: String,
    toml_key: String,
}

impl Config {
    pub fn new(
        args: ArgMatches,
        env_prefix: &'static str,
        env: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
        files: impl IntoIterator<Item = (impl Into<PathBuf>, impl Into<String>)>,
    ) -> Result<Self> {
        let env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        let files = files
            .into_iter()
            .map(|(path, contents)| {
                contents
                    .into()
                    .parse::<Table>()
                    .map(|table| (path.into(), table))
            })
            .collect::<std::result::Result<_, _>>()?;
        Ok(Self {
            args,
            env_prefix,
            env,
            files,
        })
    }

    fn get_internal<T>(&self, key: &str) -> Result<result::Result<T, KeyNames>>
    where
        T: FromStr + for<'a> Deserialize<'a>,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        let mut args_result = self.args.try_get_one::<String>(key);
        if let Err(MatchesError::UnknownArgument { .. }) = args_result {
            args_result = Ok(None);
        }
        let mut value = args_result
            .with_context(|| {
                format!("error getting matches data for command-line option `--{key}`")
            })?
            .map(String::as_str)
            .map(T::from_str)
            .transpose()
            .with_context(|| format!("error parsing command-line option `--{key}`"))?;
        if let Some(value) = value {
            return Ok(Ok(value));
        }

        let env_key: String = self
            .env_prefix
            .chars()
            .chain(key.chars())
            .map(|c| match c {
                '-' => '_',
                c => c.to_ascii_uppercase(),
            })
            .collect();
        value = self
            .env
            .get(&env_key)
            .map(String::as_str)
            .map(T::from_str)
            .transpose()
            .with_context(|| format!("error parsing environment variable `{env_key}`"))?;
        if let Some(value) = value {
            return Ok(Ok(value));
        }

        let toml_key: String = key
            .chars()
            .map(|c| match c {
                '-' => '_',
                c => c,
            })
            .collect();
        for (path, table) in &self.files {
            if let Some(value) = table.get(&toml_key) {
                return T::deserialize(value.clone())
                    .map(Result::Ok)
                    .with_context(|| {
                        format!(
                            "error parsing value for key `{toml_key}` in config file `{}`",
                            path.to_string_lossy()
                        )
                    });
            }
        }

        Ok(Err(KeyNames {
            key: key.to_string(),
            env_key,
            toml_key,
        }))
    }

    pub fn get<T>(&self, key: &str) -> Result<T>
    where
        T: FromStr + for<'a> Deserialize<'a>,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        match self.get_internal(key) {
            Err(err) => Err(err),
            Ok(Ok(v)) => Ok(v),
            Ok(Err(KeyNames {
                key,
                env_key,
                toml_key,
            })) => Err(anyhow!(
                "config value `{key}` must be set via `--{key}` command-line option, \
                `{env_key}` environment variable, or `{toml_key}` key in config file"
            )),
        }
    }

    pub fn get_or<T>(&self, key: &str, default: T) -> Result<T>
    where
        T: FromStr + for<'a> Deserialize<'a>,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        self.get_internal(key).map(|v| v.unwrap_or(default))
    }

    pub fn get_or_else<T, F>(&self, key: &str, mut default: F) -> Result<T>
    where
        T: FromStr + for<'a> Deserialize<'a>,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
        F: FnMut() -> T,
    {
        self.get_internal(key)
            .map(|v| v.unwrap_or_else(|_| default()))
    }

    pub fn get_option<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: FromStr + for<'a> Deserialize<'a>,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        self.get_internal(key).map(Result::ok)
    }

    pub fn get_flag<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: From<bool> + for<'a> Deserialize<'a>,
    {
        let mut args_result = self.args.try_get_one::<bool>(key);
        if let Err(MatchesError::UnknownArgument { .. }) = args_result {
            args_result = Ok(None);
        }
        if let Ok(Some(_)) = args_result {
            if self.args.value_source(key).unwrap() == ValueSource::DefaultValue {
                args_result = Ok(None);
            }
        }
        let mut value = args_result?.copied().map(T::from);
        if value.is_some() {
            return Ok(value);
        }

        let env_key: String = self
            .env_prefix
            .chars()
            .chain(key.chars())
            .map(|c| match c {
                '-' => '_',
                c => c.to_ascii_uppercase(),
            })
            .collect();
        value = self
            .env
            .get(&env_key)
            .map(String::as_str)
            .map(bool::from_str)
            .transpose()
            .with_context(|| format!("error parsing environment variable `{env_key}`"))?
            .map(T::from);

        if value.is_some() {
            return Ok(value);
        }

        let toml_key: String = key
            .chars()
            .map(|c| match c {
                '-' => '_',
                c => c,
            })
            .collect();
        for (path, table) in &self.files {
            if let Some(value) = table.get(&toml_key) {
                return Some(T::deserialize(value.clone()))
                    .transpose()
                    .with_context(|| {
                        format!(
                            "error parsing value for key `{toml_key}` in config file `{}`",
                            path.to_string_lossy(),
                        )
                    });
            }
        }

        Ok(None)
    }
}

pub struct ConfigBuilder {
    command: Command,
    env_var_prefix: &'static str,
}

impl ConfigBuilder {
    pub fn new(command: clap::Command, env_var_prefix: &'static str) -> Result<Self> {
        let command = command
            .styles(maelstrom_util::clap::styles())
            .after_help(format!(
                "Configuration values can be specified in three ways: fields in a config file, \
                environment variables, or command-line options. Command-line options have the \
                highest precendence, followed by environment variables.\n\
                \n\
                The configuration value 'config_value' would be set via the '--config-value' \
                command-line option, the {env_var_prefix}_CONFIG_VALUE environment variable, \
                and the 'config_value' key in a configuration file."))
            .arg(
                Arg::new("config-file")
                    .long("config-file")
                    .short('c')
                    .value_name("PATH")
                    .action(ArgAction::Set)
                    .help(
                        "Configuration file. Values set in the configuration file will be overridden by \
                        values set through environment variables and values set on the command line."
                    )
            )
            .arg(
                Arg::new("print-config")
                    .long("print-config")
                    .short('P')
                    .action(ArgAction::SetTrue)
                    .help("Print configuration and exit."),
            );

        Ok(Self {
            command,
            env_var_prefix,
        })
    }

    fn env_var_from_field(&self, field: &'static str) -> String {
        self.env_var_prefix
            .chars()
            .chain(iter::once('_'))
            .chain(field.chars())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }

    pub fn value(
        mut self,
        field: &'static str,
        short: char,
        value_name: &'static str,
        help: &'static str,
    ) -> Self {
        fn name_from_field(field: &'static str) -> String {
            field
                .chars()
                .map(|c| if c == '_' { '-' } else { c })
                .collect()
        }

        let name = name_from_field(field);
        let env_var = self.env_var_from_field(field);
        self.command = self.command.arg(
            Arg::new(name.clone())
                .long(name)
                .short(short)
                .value_name(value_name)
                .action(ArgAction::Set)
                .help(format!("{help} [env: {env_var}]")),
        );
        self
    }

    pub fn build(self) -> Command {
        self.command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};
    use indoc::indoc;

    fn get_config() -> Config {
        let args = Command::new("command")
            .arg(Arg::new("key-1").long("key-1").action(ArgAction::Set))
            .arg(
                Arg::new("int-key-1")
                    .long("int-key-1")
                    .action(ArgAction::Set),
            )
            .arg(
                Arg::new("bool-key-1")
                    .long("bool-key-1")
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("bool-key-2")
                    .long("bool-key-2")
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("bool-key-3")
                    .long("bool-key-3")
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("bool-key-4")
                    .long("bool-key-4")
                    .action(ArgAction::SetTrue),
            )
            .get_matches_from([
                "command",
                "--key-1=value-1",
                "--int-key-1=1",
                "--bool-key-1",
            ]);
        Config::new(
            args,
            "prefix_",
            [
                ("PREFIX_KEY_2", "value-2"),
                ("PREFIX_INT_KEY_2", "2"),
                ("PREFIX_BOOL_KEY_2", "true"),
            ],
            [
                (
                    "config-1.toml",
                    indoc! {r#"
                        key_3 = "value-3"
                        int_key_3 = 3
                        bool_key_3 = true
                    "#},
                ),
                (
                    "config-2.toml",
                    indoc! {r#"
                        key_4 = "value-4"
                        int_key_4 = 4
                        bool_key_4 = true
                    "#},
                ),
            ],
        )
        .unwrap()
    }

    #[test]
    fn string_values() {
        let config = get_config();
        assert_eq!(
            config.get::<String>("key-1").unwrap(),
            "value-1".to_string()
        );
        assert_eq!(
            config.get::<String>("key-2").unwrap(),
            "value-2".to_string()
        );
        assert_eq!(
            config.get::<String>("key-3").unwrap(),
            "value-3".to_string()
        );
        assert_eq!(
            config.get::<String>("key-4").unwrap(),
            "value-4".to_string()
        );
    }

    #[test]
    fn int_values() {
        let config = get_config();
        assert_eq!(config.get::<i32>("int-key-1").unwrap(), 1);
        assert_eq!(config.get::<i32>("int-key-2").unwrap(), 2);
        assert_eq!(config.get::<i32>("int-key-3").unwrap(), 3);
        assert_eq!(config.get::<i32>("int-key-4").unwrap(), 4);
    }

    #[test]
    fn bool_values() {
        let config = get_config();
        assert_eq!(config.get_flag("bool-key-1").unwrap(), Some(true));
        assert_eq!(config.get_flag("bool-key-2").unwrap(), Some(true));
        assert_eq!(config.get_flag("bool-key-3").unwrap(), Some(true));
        assert_eq!(config.get_flag("bool-key-4").unwrap(), Some(true));
    }
}
