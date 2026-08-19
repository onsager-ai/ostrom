use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::sha256_hex;

/// Deliberately tolerant of unknown fields.
///
/// `deny_unknown_fields` here meant every `gh api .../branches` response was
/// rejected, because GitHub sends `protected` on each branch and `url` on each
/// commit. Dispatch reported that as `branch-listing-degraded` and refused to
/// dispatch anything for two days — the JSON was valid and the credentials were
/// fine; only our deserialiser disagreed.
///
/// A remote API we do not control will add fields. Denying unknown ones asserts
/// that GitHub's response shape is frozen, which is not a claim we can make.
/// `valid()` below is what actually guards the data we depend on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranch {
    pub name: String,
    pub commit: RemoteCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCommit {
    pub sha: String,
}

impl RemoteBranch {
    #[must_use]
    pub fn valid(&self) -> bool {
        !self.name.is_empty()
            && self.commit.sha.len() == 40
            && self
                .commit
                .sha
                .chars()
                .all(|character| character.is_ascii_hexdigit())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchListingOutcome {
    Matched,
    ProvenExhaustiveNoMatch,
    ListingDegraded,
}

impl BranchListingOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::ProvenExhaustiveNoMatch => "proven-exhaustive-no-match",
            Self::ListingDegraded => "listing-degraded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchListing {
    pub outcome: BranchListingOutcome,
    pub page_count: usize,
    pub branch_count: usize,
    pub matched: Option<RemoteBranch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("listing-degraded: {detail}")]
pub struct BranchListingFault {
    pub page_count: usize,
    pub branch_count: usize,
    pub detail: String,
}

/// Resolve only the authoritative branch key. A numeric suffix, prefix, or
/// any other near miss remains a proven negative after exhaustive listing.
pub fn resolve_exact_branch(
    pages: &[Vec<RemoteBranch>],
    expected: &str,
    page_size: usize,
    page_limit: usize,
) -> Result<BranchListing, BranchListingFault> {
    let mut branch_count = 0;
    let mut exhaustive = false;
    for (index, page) in pages.iter().enumerate() {
        let page_number = index + 1;
        if page.len() > page_size || page.iter().any(|branch| !branch.valid()) {
            return Err(BranchListingFault {
                page_count: index,
                branch_count,
                detail: format!("page {page_number} response was malformed"),
            });
        }
        branch_count += page.len();
        if page.len() < page_size {
            exhaustive = true;
            break;
        }
        if page_number == page_limit {
            return Err(BranchListingFault {
                page_count: page_number,
                branch_count,
                detail: format!(
                    "listing reached page limit {page_limit} without proving exhaustion"
                ),
            });
        }
    }
    if exhaustive {
        let matched = pages
            .iter()
            .flatten()
            .find(|branch| branch.name == expected)
            .cloned();
        return Ok(BranchListing {
            outcome: if matched.is_some() {
                BranchListingOutcome::Matched
            } else {
                BranchListingOutcome::ProvenExhaustiveNoMatch
            },
            page_count: pages.len(),
            branch_count,
            matched,
        });
    }
    Err(BranchListingFault {
        page_count: pages.len(),
        branch_count,
        detail: format!("page {} was not read", pages.len() + 1),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkOrder {
    pub schema_version: u32,
    pub item_id: String,
    pub repository: String,
    pub item_ref: String,
    pub branch_name: String,
    pub spec: String,
    pub acceptance_criteria: Vec<String>,
    pub constraints: Vec<String>,
    pub order_id: String,
    pub created_at: String,
    pub cost_ceiling_usd: Value,
    pub token_ceiling: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkOrderError {
    #[error("invalid schema_version 1 work order")]
    Invalid,
}

impl WorkOrder {
    pub fn from_json(bytes: &[u8]) -> Result<Self, WorkOrderError> {
        let order: Self = serde_json::from_slice(bytes).map_err(|_| WorkOrderError::Invalid)?;
        if !order.valid() {
            return Err(WorkOrderError::Invalid);
        }
        Ok(order)
    }

    #[must_use]
    pub fn item_hash(&self) -> String {
        sha256_hex(self.item_id.as_bytes())
    }

    #[must_use]
    pub fn cost(&self) -> f64 {
        self.cost_ceiling_usd.as_f64().unwrap_or_default()
    }

    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.token_ceiling.as_u64().unwrap_or_default()
    }

    fn valid(&self) -> bool {
        self.schema_version == 1
            && is_lower_hex(&self.order_id, 64)
            && !self.item_id.is_empty()
            && valid_repository(&self.repository)
            && !self.item_ref.is_empty()
            && valid_branch(&self.branch_name)
            && !self.spec.is_empty()
            && !self.acceptance_criteria.is_empty()
            && self
                .acceptance_criteria
                .iter()
                .all(|value| !value.is_empty())
            && self.constraints.iter().all(|value| !value.is_empty())
            && valid_created_at(&self.created_at)
            && self.cost() > 0.0
            && self.token_ceiling.as_u64().is_some_and(|tokens| tokens > 0)
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
}

fn valid_repository(repository: &str) -> bool {
    if repository.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = repository.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    )
}

fn valid_branch(branch: &str) -> bool {
    !branch.contains("..")
        && branch
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && branch.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '-')
        })
}

fn valid_created_at(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

#[cfg(test)]
mod tests {
    /// The exact payload GitHub returns, which `deny_unknown_fields` rejected.
    ///
    /// `protected` on the branch and `url` on the commit are always present in
    /// a real `gh api .../branches` response. Rejecting them stopped dispatch
    /// for two days while the credentials and the JSON were both fine.
    #[test]
    fn a_real_github_branch_payload_deserialises() {
        let payload = br#"[{
            "name": "placeholder/branch",
            "commit": {
              "sha": "0123456789abcdef0123456789abcdef01234567",
              "url": "https://api.github.com/repos/placeholder-org/alpha/commits/0123456789abcdef0123456789abcdef01234567"
            },
            "protected": false
        }]"#;
        let branches: Vec<RemoteBranch> =
            serde_json::from_slice(payload).expect("a real GitHub branch payload must deserialise");
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].name, "placeholder/branch");
        assert!(branches[0].valid());
    }

    /// A field GitHub has not invented yet must not break dispatch either.
    #[test]
    fn an_unforeseen_field_does_not_break_the_listing() {
        let payload = br#"[{
            "name": "placeholder/branch",
            "commit": {"sha": "0123456789abcdef0123456789abcdef01234567"},
            "some_field_github_adds_later": {"nested": true}
        }]"#;
        let branches: Vec<RemoteBranch> =
            serde_json::from_slice(payload).expect("unknown fields must be tolerated");
        assert!(branches[0].valid());
    }

    use super::*;

    fn branch(name: &str) -> RemoteBranch {
        RemoteBranch {
            name: name.to_owned(),
            commit: RemoteCommit {
                sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            },
        }
    }

    #[test]
    fn exact_branch_keys_never_fall_back_to_number_or_prefix() {
        let pages = vec![vec![
            branch("ostrom/267-near-miss"),
            branch("ostrom/26"),
            branch("267"),
        ]];
        let listing = resolve_exact_branch(&pages, "ostrom/267-exact", 100, 100)
            .expect("short page proves exhaustion");
        assert_eq!(
            listing.outcome,
            BranchListingOutcome::ProvenExhaustiveNoMatch
        );
        assert!(listing.matched.is_none());
    }

    #[test]
    fn incomplete_listing_is_degraded_not_a_negative() {
        let full = (0..100)
            .map(|number| branch(&format!("placeholder/{number}")))
            .collect::<Vec<_>>();
        let fault = resolve_exact_branch(&[full], "ostrom/267-exact", 100, 100)
            .expect_err("a full page does not prove exhaustion");
        assert_eq!(fault.page_count, 1);
        assert_eq!(fault.branch_count, 100);
        assert!(fault.to_string().starts_with("listing-degraded:"));
    }
}
