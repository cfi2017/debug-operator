use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, bail};

pub fn run_cli_if_requested() -> anyhow::Result<bool> {
    let mut args = std::env::args();
    let _program = args.next();
    let Some(command) = args.next() else {
        return Ok(false);
    };

    match command.as_str() {
        "install-patcher" => {
            let destination = args
                .next()
                .context("install-patcher requires DESTINATION")?;
            let writable_directories = args.collect::<Vec<_>>();
            install_patcher(Path::new(&destination), &writable_directories)?;
            Ok(true)
        }
        "patch-binary" => {
            let source = args.next().context("patch-binary requires SOURCE")?;
            let output = args.next().context("patch-binary requires OUTPUT")?;
            let find_hex = args.next().context("patch-binary requires FIND_HEX")?;
            let replace_hex = args.next().context("patch-binary requires REPLACE_HEX")?;
            let expected: usize = args
                .next()
                .context("patch-binary requires EXPECTED_MATCHES")?
                .parse()
                .context("EXPECTED_MATCHES must be an integer")?;
            if args.next().is_some() {
                bail!("patch-binary accepts exactly five arguments");
            }
            patch_binary(
                Path::new(&source),
                Path::new(&output),
                &find_hex,
                &replace_hex,
                expected,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn install_patcher(destination: &Path, writable_directories: &[String]) -> anyhow::Result<()> {
    let source = std::env::current_exe().context("locate patcher executable")?;
    let parent = destination
        .parent()
        .context("patcher destination has no parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o777))?;
    fs::copy(source, destination).context("copy patcher executable")?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    for directory in writable_directories {
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o777))?;
    }
    Ok(())
}

pub fn patch_binary(
    source: &Path,
    output: &Path,
    find_hex: &str,
    replace_hex: &str,
    expected_matches: usize,
) -> anyhow::Result<()> {
    let find = decode_hex(find_hex).context("decode findHex")?;
    let replacement_hex = if let Some(path) = replace_hex.strip_prefix('@') {
        fs::read_to_string(path).with_context(|| format!("read replacement hex from {path}"))?
    } else {
        replace_hex.to_string()
    };
    let replacement = decode_hex(&replacement_hex).context("decode replaceHex")?;
    if find.is_empty() {
        bail!("findHex must not be empty");
    }

    let input = fs::read(source).with_context(|| format!("read {}", source.display()))?;
    let mut matches = Vec::new();
    let mut search_from = 0;
    while search_from + find.len() <= input.len() {
        if input[search_from..].starts_with(&find) {
            matches.push(search_from);
            search_from += find.len();
        } else {
            search_from += 1;
        }
    }
    if matches.len() != expected_matches {
        bail!(
            "expected {expected_matches} matches for findHex in {}, found {}",
            source.display(),
            matches.len()
        );
    }

    let mut patched = Vec::with_capacity(
        input.len() + matches.len() * replacement.len().saturating_sub(find.len()),
    );
    let mut cursor = 0;
    for index in matches {
        patched.extend_from_slice(&input[cursor..index]);
        patched.extend_from_slice(&replacement);
        cursor = index + find.len();
    }
    patched.extend_from_slice(&input[cursor..]);

    let parent = output.parent().context("patch output has no parent")?;
    fs::create_dir_all(parent)?;
    fs::write(output, patched).with_context(|| format!("write {}", output.display()))?;
    let mode = fs::metadata(source)?.permissions().mode();
    fs::set_permissions(output, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn decode_hex(value: &str) -> anyhow::Result<Vec<u8>> {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    Ok(hex::decode(compact)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_exact_match_count_and_preserves_mode() {
        let directory =
            std::env::temp_dir().join(format!("debug-operator-patch-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source");
        let output = directory.join("output");
        fs::write(&source, b"prefix ABC suffix ABC").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

        patch_binary(&source, &output, "41 42 43", "00ff", 2).unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"prefix \0\xff suffix \0\xff");
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o751
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn refuses_unexpected_match_count() {
        let directory =
            std::env::temp_dir().join(format!("debug-operator-patch-count-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source");
        fs::write(&source, b"ABC ABC").unwrap();

        let error =
            patch_binary(&source, &directory.join("output"), "414243", "00", 1).unwrap_err();
        assert!(error.to_string().contains("found 2"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn reads_replacement_hex_from_file() {
        let directory = std::env::temp_dir().join(format!(
            "debug-operator-patch-replacement-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source");
        let replacement = directory.join("replacement.hex");
        let output = directory.join("output");
        fs::write(&source, b"ABC").unwrap();
        fs::write(&replacement, "00 ff 01\n").unwrap();

        patch_binary(
            &source,
            &output,
            "414243",
            &format!("@{}", replacement.display()),
            1,
        )
        .unwrap();

        assert_eq!(fs::read(output).unwrap(), [0, 255, 1]);
        let _ = fs::remove_dir_all(directory);
    }
}
