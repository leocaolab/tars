//! Names the directory a tenant's files sit under, `<root>/tenant=<escaped>`, and reads a tenant
//! back from one. The Parquet log lays its partitions out under it, and so do other stores that
//! keep a tenant's files beside them (the agent's live transcripts), so a tenant has one directory
//! name everywhere.

use std::path::{Path, PathBuf};
use std::str::Utf8Error;

use tars_types::{ParseTenantIdError, TenantId};

/// Starts the name of every tenant directory; the escaped tenant follows it.
pub const TENANT_PREFIX: &str = "tenant=";

/// Everything but `[a-z0-9_-]` is escaped, so `.` / `..` / `/` never survive, and two tenants
/// that differ only in case get two directories on a file system that ignores case.
const TENANT_ESCAPE: &percent_encoding::AsciiSet =
    &with_uppercase(percent_encoding::NON_ALPHANUMERIC.remove(b'_').remove(b'-'));

/// Returns `set` with `A`-`Z` added. The escape writes its hex digits in upper case, so the
/// escaped text holds no lower-case letter that a letter's escape could fold into.
const fn with_uppercase(set: percent_encoding::AsciiSet) -> percent_encoding::AsciiSet {
    let mut set = set;
    let mut letter = b'A';
    while letter <= b'Z' {
        set = set.add(letter);
        letter += 1;
    }
    set
}

/// Returns `<root>/tenant=<escaped tenant>`.
pub fn tenant_dir(root: &Path, tenant: &TenantId) -> PathBuf {
    let escaped = percent_encoding::utf8_percent_encode(tenant.as_str(), TENANT_ESCAPE);
    root.join(format!("{TENANT_PREFIX}{escaped}"))
}

/// Returns the tenant whose directory name is `TENANT_PREFIX` followed by `escaped`, the inverse
/// of [`tenant_dir`].
///
/// # Errors
///
/// [`ParseTenantDirError`] when `escaped` unescapes to text that is not UTF-8 or not a tenant id.
pub fn parse_tenant_dir(escaped: &str) -> Result<TenantId, ParseTenantDirError> {
    percent_encoding::percent_decode_str(escaped)
        .decode_utf8()
        .map_err(ParseTenantDirError::NotUtf8)?
        .parse()
        .map_err(ParseTenantDirError::NotATenant)
}

/// Why a directory name names no tenant.
#[derive(Debug, thiserror::Error)]
pub enum ParseTenantDirError {
    #[error("the unescaped name is not UTF-8")]
    NotUtf8(#[source] Utf8Error),
    #[error("the unescaped name is not a tenant id")]
    NotATenant(#[source] ParseTenantIdError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_reads_back_from_the_directory_it_is_written_under() {
        for tenant in ["acme", "Acme", "globex/eu 1", "../x", "é"] {
            let tenant: TenantId = tenant.parse().unwrap();
            let dir = tenant_dir(Path::new("root"), &tenant);
            let name = dir.file_name().unwrap().to_str().unwrap();
            let escaped = name.strip_prefix(TENANT_PREFIX).unwrap();

            assert_eq!(parse_tenant_dir(escaped).unwrap(), tenant);
        }
    }

    #[test]
    fn an_empty_or_non_utf8_name_names_no_tenant() {
        assert!(matches!(
            parse_tenant_dir(""),
            Err(ParseTenantDirError::NotATenant(_))
        ));
        assert!(matches!(
            parse_tenant_dir("%FF"),
            Err(ParseTenantDirError::NotUtf8(_))
        ));
    }
}
