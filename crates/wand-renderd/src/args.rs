//! 命令行参数。Render 只有一种常驻形态：`wand-render -c <configPath>`。

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
  Run { config_path: PathBuf },
  Version,
  Help,
}

pub const HELP: &str = "\
wand-render — Wand 常驻 Render 守护进程（PTY / 输出 journal / VT 屏幕模型）

用法：
  wand-render -c <configPath>     启动（socket 路径由 configPath 派生）
  wand-render --config <path>     同上
  wand-render --version           打印版本
  wand-render --help              显示本帮助
";

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
  let mut config_path: Option<PathBuf> = None;
  let mut iter = args.into_iter();
  while let Some(argument) = iter.next() {
    match argument.as_str() {
      "-c" | "--config" => {
        let value = iter
          .next()
          .ok_or_else(|| format!("{argument} requires a config path"))?;
        config_path = Some(PathBuf::from(value));
      }
      "--version" | "-V" | "-v" => return Ok(Command::Version),
      "--help" | "-h" => return Ok(Command::Help),
      other => {
        if let Some(value) = other.strip_prefix("--config=") {
          config_path = Some(PathBuf::from(value));
        } else {
          return Err(format!("unrecognized argument {other}"));
        }
      }
    }
  }
  match config_path {
    Some(path) => Ok(Command::Run { config_path: path }),
    None => Err("missing -c <configPath>".to_string()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|value| value.to_string()).collect()
  }

  #[test]
  fn parses_short_and_long_config() {
    assert_eq!(
      parse(args(&["-c", "/tmp/x/config.json"])),
      Ok(Command::Run {
        config_path: PathBuf::from("/tmp/x/config.json")
      })
    );
    assert_eq!(
      parse(args(&["--config", "/tmp/x/config.json"])),
      Ok(Command::Run {
        config_path: PathBuf::from("/tmp/x/config.json")
      })
    );
    assert_eq!(
      parse(args(&["--config=/tmp/x/config.json"])),
      Ok(Command::Run {
        config_path: PathBuf::from("/tmp/x/config.json")
      })
    );
  }

  #[test]
  fn parses_version_and_help() {
    assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    assert_eq!(parse(args(&["-V"])), Ok(Command::Version));
    assert_eq!(parse(args(&["--help"])), Ok(Command::Help));
  }

  #[test]
  fn rejects_missing_and_unknown_arguments() {
    assert!(parse(args(&[])).is_err());
    assert!(parse(args(&["-c"])).is_err());
    assert!(parse(args(&["--nope"])).is_err());
  }
}
