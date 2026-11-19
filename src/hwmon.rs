use {
    std::{
        fs::{self, File, OpenOptions},
        io::{self, Read, Write},
        path::{Path, PathBuf},
    },
    crate::error::{Error, Result},
};

/// The directory that holds one entry per hwmon chip
const DEFAULT_ROOT: &str = "/sys/class/hwmon";

/// Most bytes read from one sysfs attribute. A number and its line ending
/// fit with room to spare.
const ATTRIBUTE_CAP: u64 = 64;

/// The directory of hwmon chips. A chip is found by the text of its `name`
/// file, never by its `hwmonN` number, which can change between boots.
#[derive(Clone, Debug)]
pub struct HwmonRoot(PathBuf);

impl Default for HwmonRoot {
    fn default() -> Self {
        Self(PathBuf::from(DEFAULT_ROOT))
    }
}

impl HwmonRoot {
    #[cfg(test)]
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// The directory of the one chip whose `name` file holds `chip`, after
    /// trimming. An entry without a `name` file is skipped.
    pub fn resolve(&self, chip: &str) -> Result<PathBuf> {
        let mut matches = Vec::new();

        for entry in self.entries()? {
            let Some(name) = read_name(&entry)? else {
                continue;
            };

            if name == chip {
                matches.push(entry);
            }
        }

        match matches.len() {
            0 => Err(Error::HwmonChipNotFound { chip: chip.to_owned(), root: self.0.clone() }),
            1 => Ok(matches.remove(0)),
            _ => Err(Error::HwmonChipAmbiguous { chip: chip.to_owned(), paths: matches }),
        }
    }

    /// Every entry of the root, sorted, so that an error lists paths in the
    /// same order each time
    fn entries(&self) -> Result<Vec<PathBuf>> {
        let io_error = |e| Error::Io { path: self.0.clone(), source: e };
        let mut entries = Vec::new();

        for entry in fs::read_dir(&self.0).map_err(io_error)? {
            entries.push(entry.map_err(io_error)?.path());
        }

        entries.sort();

        Ok(entries)
    }
}

/// The trimmed text of the `name` file of `entry`, or `None` when it has none
fn read_name(entry: &Path) -> Result<Option<String>> {
    let path = entry.join("name");

    match read_attribute(&path) {
        Ok(name) => Ok(Some(name.trim().to_owned())),
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read at most [`ATTRIBUTE_CAP`] bytes of a sysfs attribute as text
fn read_attribute(path: &Path) -> Result<String> {
    let io_error = |e| Error::Io { path: path.to_owned(), source: e };
    let mut bytes = Vec::new();

    File::open(path).map_err(io_error)?
        .take(ATTRIBUTE_CAP)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;

    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read a sysfs attribute that holds one decimal integer
pub(crate) fn read_number(path: &Path) -> Result<i64> {
    let text = read_attribute(path)?;
    let value = text.trim();

    value.parse().map_err(|e| Error::HwmonParse {
        path: path.to_owned(),
        value: value.to_owned(),
        source: e,
    })
}

/// Write `value` to a sysfs attribute. A missing attribute is an error and is
/// not created, because a chip that went away must not gain a plain file.
pub(crate) fn write_number(path: &Path, value: u32) -> Result<()> {
    let io_error = |e| Error::Io { path: path.to_owned(), source: e };

    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(io_error)?
        .write_all(value.to_string().as_bytes())
        .map_err(io_error)
}

#[cfg(test)]
mod tests {
    use {
        std::fs,
        super::{ATTRIBUTE_CAP, HwmonRoot, read_number, write_number},
        crate::{command::testing::TempDir, error::Error},
    };

    /// A hwmon tree with one directory per `(entry, name)`, each holding a
    /// `name` file
    fn tree(chips: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new();

        for (entry, name) in chips {
            fs::create_dir(dir.0.join(entry)).unwrap();
            fs::write(dir.0.join(entry).join("name"), format!("{}\n", name)).unwrap();
        }

        dir
    }

    fn root_of(dir: &TempDir) -> HwmonRoot {
        HwmonRoot::new(dir.0.clone())
    }

    #[test]
    fn resolves_a_chip_by_its_name() {
        let dir = tree(&[("hwmon0", "nvme"), ("hwmon1", "nct6798")]);

        assert_eq!(root_of(&dir).resolve("nct6798").unwrap(), dir.0.join("hwmon1"));
    }

    #[test]
    fn resolves_a_chip_whose_number_changed() {
        let dir = tree(&[("hwmon0", "nct6798"), ("hwmon1", "nvme")]);

        assert_eq!(root_of(&dir).resolve("nct6798").unwrap(), dir.0.join("hwmon0"));
    }

    #[test]
    fn two_chips_of_one_name_fail_with_both_paths() {
        let dir = tree(&[("hwmon0", "nvme"), ("hwmon1", "nct6798"), ("hwmon2", "nvme")]);

        let error = root_of(&dir).resolve("nvme").err().expect("two chips named nvme fail");

        match error {
            Error::HwmonChipAmbiguous { chip, paths } => {
                assert_eq!(chip, "nvme");
                assert_eq!(paths, [dir.0.join("hwmon0"), dir.0.join("hwmon2")]);
            }
            other => panic!("expected an ambiguous chip, got {:?}", other),
        }
    }

    #[test]
    fn missing_chip_fails_with_its_name() {
        let dir = tree(&[("hwmon0", "nvme")]);

        let error = root_of(&dir).resolve("k10temp").err().expect("no chip named k10temp");

        assert!(matches!(&error, Error::HwmonChipNotFound { chip, .. } if chip == "k10temp"), "{:?}", error);
        assert!(error.to_string().contains("\"k10temp\""), "{}", error);
    }

    #[test]
    fn reads_a_number_with_its_line_ending() {
        let dir = TempDir::new();
        let path = dir.write("temp1_input", "-1500\n");

        assert_eq!(read_number(&path).unwrap(), -1500);
    }

    #[test]
    fn parse_error_names_the_path_and_the_value() {
        let dir = TempDir::new();
        let path = dir.write("pwm2", "fast\n");

        match read_number(&path) {
            Err(Error::HwmonParse { path: failed, value, .. }) => {
                assert_eq!(failed, path);
                assert_eq!(value, "fast");
            }
            other => panic!("expected a parse error, got {:?}", other),
        }
    }

    #[test]
    fn reads_at_most_the_cap() {
        let dir = TempDir::new();
        let path = dir.write("pwm2", &"1".repeat(ATTRIBUTE_CAP as usize + 1));

        match read_number(&path) {
            Err(Error::HwmonParse { value, .. }) => assert_eq!(value.len(), ATTRIBUTE_CAP as usize),
            other => panic!("expected a parse error, got {:?}", other),
        }
    }

    #[test]
    fn read_error_names_the_path() {
        let dir = TempDir::new();
        let path = dir.0.join("pwm2");

        assert!(matches!(read_number(&path), Err(Error::Io { path: failed, .. }) if failed == path));
    }

    #[test]
    fn write_replaces_the_whole_value() {
        let dir = TempDir::new();
        let path = dir.write("pwm2_enable", "255\n");

        write_number(&path, 5).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "5");
    }

    #[test]
    fn write_to_a_missing_attribute_fails_without_creating_it() {
        let dir = TempDir::new();
        let path = dir.0.join("pwm2");

        let result = write_number(&path, 255);

        assert!(matches!(&result, Err(Error::Io { path: failed, .. }) if *failed == path), "{:?}", result);
        assert!(!path.exists());
    }
}
