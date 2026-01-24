use meta_cli::config;

/// Get project directories - uses passed-in list if non-empty, otherwise reads local .meta
pub(crate) fn get_project_directories_with_fallback(projects: &[String]) -> anyhow::Result<Vec<String>> {
    if !projects.is_empty() {
        // Use the projects list from meta_cli (supports --recursive)
        Ok(projects.to_vec())
    } else {
        // Fall back to reading local .meta file
        get_project_directories()
    }
}

pub(crate) fn get_project_directories() -> anyhow::Result<Vec<String>> {
    let cwd = std::env::current_dir()?;
    let tree = match config::walk_meta_tree(&cwd, Some(0)) {
        Ok(t) => t,
        Err(_) => return Ok(vec![".".to_string()]),
    };
    let mut dirs = vec![".".to_string()];
    let mut paths: Vec<String> = tree.iter().map(|n| n.info.path.clone()).collect();
    paths.sort();
    dirs.extend(paths);
    Ok(dirs)
}

/// Get all repository directories for snapshot operations (recursive by default)
pub(crate) fn get_all_repo_directories(projects: &[String]) -> anyhow::Result<Vec<String>> {
    if !projects.is_empty() {
        return Ok(projects.to_vec());
    }

    let cwd = std::env::current_dir()?;
    let tree = match config::walk_meta_tree(&cwd, None) {
        Ok(t) => t,
        Err(_) => return Ok(vec![".".to_string()]),
    };
    let mut dirs = vec![".".to_string()];
    dirs.extend(config::flatten_meta_tree(&tree));
    Ok(dirs)
}
