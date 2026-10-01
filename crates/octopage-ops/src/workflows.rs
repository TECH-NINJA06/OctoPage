use bytes::Bytes;
use octopage::{Database, Error, ObjectId, Result, Transport};
use octopage_git::{Commit, EntryMode, Object, RefUpdate, Tree, TreeEntry};

use crate::records::{self, read_ref};
use crate::walk::{Commits, database_root, root_tree};

/// Where the workflow goes in the repository.
pub const MAINTENANCE_PATH: &str = ".github/workflows/octopage-maintenance.yml";
pub fn maintenance_workflow(cli_source: &str, cli_ref: &str) -> String {
    TEMPLATE
        .replace("{{CLI_SOURCE}}", cli_source)
        .replace("{{CLI_REF}}", cli_ref)
}

const TEMPLATE: &str = r#"# OctoPage maintenance, installed by `octopage workflows install`.
#
# Weekly: removes abandoned staging refs, scans every page of every database, projects the
# repository's size (opening an issue two weeks before it reaches its budget), deletes a previous
# generation after its grace period, and rolls the databases over to a new generation when the
# size calls for it.
#
# Secrets (Settings > Secrets and variables > Actions), both optional:
#   OCTOPAGE_ADMIN_TOKEN  creates and deletes repositories, for rollovers: a fine-grained token
#                         with Administration, Contents and Workflows (read and write) on all of
#                         the owner's repositories. Without it the job only warns.
#   OCTOPAGE_PASSPHRASE   opens encrypted databases, for full page scans. Without it pages are
#                         checked for their framing only.
name: OctoPage maintenance

on:
  schedule:
    - cron: "17 3 * * 1"
  workflow_dispatch:
    inputs:
      rollover:
        description: "Roll over to a new generation: auto, always or never"
        required: false
        default: auto

permissions:
  contents: write
  issues: write

concurrency:
  group: octopage-maintenance
  cancel-in-progress: false

jobs:
  maintain:
    runs-on: ubuntu-latest
    timeout-minutes: 180
    steps:
      - name: Find the octopage CLI's commit
        id: cli
        run: |
          sha=$(git ls-remote "{{CLI_SOURCE}}" "refs/heads/{{CLI_REF}}" | cut -f1)
          test -n "$sha" || { echo "cannot read {{CLI_SOURCE}} ({{CLI_REF}})"; exit 1; }
          echo "sha=$sha" >> "$GITHUB_OUTPUT"
      - name: Cache the octopage CLI
        id: cache
        uses: actions/cache@v4
        with:
          path: ~/.cargo/bin/octopage
          key: octopage-cli-${{ steps.cli.outputs.sha }}-${{ runner.os }}
      - name: Install the octopage CLI
        if: steps.cache.outputs.cache-hit != 'true'
        run: cargo install --locked --git "{{CLI_SOURCE}}" --rev "${{ steps.cli.outputs.sha }}" octopage-cli
      - name: Maintain
        env:
          OCTOPAGE_GITHUB_TOKEN: ${{ github.token }}
          OCTOPAGE_ADMIN_TOKEN: ${{ secrets.OCTOPAGE_ADMIN_TOKEN }}
          OCTOPAGE_PASSPHRASE: ${{ secrets.OCTOPAGE_PASSPHRASE }}
          ROLLOVER: ${{ inputs.rollover || 'auto' }}
        run: >-
          octopage maintain
          --repo "$GITHUB_REPOSITORY"
          --rollover "$ROLLOVER"
          --summary "$GITHUB_STEP_SUMMARY"
"#;

/// Write `contents` at `path` on `branch`: through the database if the branch holds one (the
/// file then stays in every later commit), or as a plain commit otherwise. Needs a token that
/// may change workflows. Returns the new head.
pub async fn install<T: Transport + 'static>(
    transport: std::sync::Arc<T>,
    branch: &str,
    path: &str,
    contents: &[u8],
    unlock: Option<octopage::Unlock>,
) -> Result<ObjectId> {
    let head = read_ref(&*transport, branch).await?;
    let is_database = match head {
        Some(head) => {
            let mut commits = Commits::default();
            let (_, commit) = commits.load(&*transport, head).await?;
            database_root(&root_tree(&*transport, commit.tree).await?).is_some()
        }
        None => false,
    };
    if is_database {
        let config = octopage_pagestore::Config {
            branch: branch.to_string(),
            head_poll: None,
            unlock,
            ..octopage_pagestore::Config::default()
        };
        let store = match octopage_pagestore::PageStore::open_shared(transport.clone(), config)
            .await
        {
            Ok(store) => store,
            Err(octopage_pagestore::Error::Locked) => {
                return Err(Error::Invalid(
                    "the branch holds an encrypted database: a commit to it needs its passphrase (OCTOPAGE_PASSPHRASE)"
                        .into(),
                ));
            }
            Err(e) => return Err(e.into()),
        };
        let db = Database::from_store(store, octopage::Config::default())?;
        return db
            .put_file(path, Some(Bytes::copy_from_slice(contents)))
            .await;
    }
    plain_commit(&*transport, branch, head, path, contents).await
}

/// A commit on `branch` (or a new branch) that sets one file.
async fn plain_commit<T: Transport>(
    t: &T,
    branch: &str,
    head: Option<ObjectId>,
    path: &str,
    contents: &[u8],
) -> Result<ObjectId> {
    let mut objects: Vec<Object> = Vec::new();
    let top = match head {
        Some(head) => {
            let mut commits = Commits::default();
            let (_, commit) = commits.load(t, head).await?;
            root_tree(t, commit.tree).await?
        }
        None => Tree::new(),
    };
    let blob = Object::blob(contents.to_vec())?;
    let blob_id = blob.id();
    objects.push(blob);
    let parts: Vec<&str> = path.split('/').collect();
    let top = place(t, top, &parts, blob_id, &mut objects).await?;
    let tree = top.to_object()?;
    let signature = records::signature(records::now());
    let commit = Commit {
        tree: tree.id(),
        parents: head.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        message: format!("Add {path}\n").into_bytes(),
    }
    .to_object()?;
    objects.push(tree);
    objects.push(commit.clone());
    let update = match head {
        Some(old) => RefUpdate::update(branch, old, commit.id()),
        None => RefUpdate::create(branch, commit.id()),
    };
    let objects: Vec<octopage_git::NewObject> = objects.into_iter().map(Into::into).collect();
    t.push(&[update], &objects).await?;
    Ok(commit.id())
}

async fn place<T: Transport>(
    t: &T,
    mut tree: Tree,
    parts: &[&str],
    blob: ObjectId,
    objects: &mut Vec<Object>,
) -> Result<Tree> {
    let name = parts[0].as_bytes();
    if parts.len() == 1 {
        tree.insert(TreeEntry::blob(name, blob))
            .map_err(Error::from)?;
        return Ok(tree);
    }
    let child = match tree.get(name) {
        Some(e) if e.mode == EntryMode::Tree => {
            Tree::decode(records::object(t, e.id).await?.data()).map_err(Error::from)?
        }
        _ => Tree::new(),
    };
    let child = Box::pin(place(t, child, &parts[1..], blob, objects)).await?;
    let object = child.to_object()?;
    tree.insert(TreeEntry::tree(name, object.id()))
        .map_err(Error::from)?;
    objects.push(object);
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_filled_in() {
        let yaml = maintenance_workflow("https://github.com/me/octopage", "v1");
        assert!(!yaml.contains("{{CLI_"));
        assert!(yaml.contains(r#"git ls-remote "https://github.com/me/octopage" "refs/heads/v1""#));
        assert!(yaml.contains(r#"--rev "${{ steps.cli.outputs.sha }}""#));
        assert!(yaml.contains("octopage maintain"));
        assert!(yaml.contains("${{ secrets.OCTOPAGE_ADMIN_TOKEN }}"));
    }
}
