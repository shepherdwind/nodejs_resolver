//! # nodejs_resolver
//!
//! ## How to use?
//!
//! ```rust
//! // |-- node_modules
//! // |---- foo
//! // |------ index.js
//! // | src
//! // |-- foo.ts
//! // |-- foo.js
//! // | tests
//!
//! use nodejs_resolver::Resolver;
//!
//! let cwd = std::env::current_dir().unwrap();
//! let resolver = Resolver::default();
//!
//! resolver.resolve(&cwd.join("./src"), "foo");
//! // -> ResolveResult::Info(ResolverInfo {
//! //    path: PathBuf::from("<cwd>/node_modules/foo/index.js")
//! //    request: Request {
//! //       target: "",
//! //       fragment: "",
//! //       query: ""
//! //    }
//! //  })
//! //
//!
//! resolver.resolve(&cwd.join("./src"), "./foo");
//! // -> ResolveResult::Info(ResolverInfo {
//! //    path: PathBuf::from("<cwd>/src/foo.js")
//! //    request: Request {
//! //       target: "",
//! //       fragment: "",
//! //       query: ""
//! //    }
//! //  })
//! //
//! ```
//!

mod description;
mod fs;
mod kind;
mod map;
mod normalize;
mod options;
mod parse;
mod plugin;
mod resolve;
mod tsconfig;
mod tsconfig_path;
mod utils;

use dashmap::DashMap;
use description::PkgFileInfo;
use kind::PathKind;
pub use options::{AliasMap, ResolverOptions};
use plugin::{AliasFieldPlugin, AliasPlugin, ImportsFieldPlugin, Plugin, PreferRelativePlugin};

use parse::Request;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::utils::RAISE_RESOLVE_ERROR_TAG;

#[derive(Default, Debug)]
pub struct Resolver {
    pub options: ResolverOptions,
    pub unsafe_cache: Option<Arc<ResolverUnsafeCache>>,
    pub safe_cache: ResolverSafeCache,
    pub input_path: Option<PathBuf>,
    pub input_request: Option<String>,
    // /// just use under development.
    // dbg_map: DashMap<PathBuf, bool>,
}

#[derive(Default, Debug, Clone)]
pub struct ResolverUnsafeCache {
    /// key is pointed to the directory of description file.
    pub pkg_info: DashMap<PathBuf, Option<Arc<PkgFileInfo>>>,
}

#[derive(Default, Debug)]
pub struct ResolverSafeCache {
    pub(crate) target_kind: DashMap<String, PathKind>,
}

pub type ResolverError = String;

#[derive(Debug, Clone)]
pub struct ResolverInfo {
    pub path: PathBuf,
    pub request: Request,
}

impl ResolverInfo {
    pub fn from(path: PathBuf, request: Request) -> Self {
        Self { path, request }
    }

    pub fn get_path(&self) -> PathBuf {
        if self.request.target.is_empty() {
            self.path.to_path_buf()
        } else {
            self.path.join(&*self.request.target)
        }
    }

    pub fn with_path(self, path: PathBuf) -> Self {
        Self { path, ..self }
    }

    pub fn with_target(self, resolver: &Resolver, target: &str) -> Self {
        let request = self.request.with_target(resolver, target);
        Self { request, ..self }
    }

    pub fn join(&self) -> PathBuf {
        let buf = format!(
            "{}{}{}",
            self.path.display(),
            self.request.query,
            self.request.fragment,
        );
        PathBuf::from(buf)
    }
}

#[derive(Debug)]
pub enum ResolverResult {
    Info(ResolverInfo),
    Ignored,
}

#[derive(Debug)]
pub(crate) enum ResolverStats {
    Success(ResolverResult),
    Resolving(ResolverInfo),
    Error((ResolverError, ResolverInfo)),
}

impl ResolverStats {
    pub fn and_then<F: FnOnce(ResolverInfo) -> ResolverStats>(self, op: F) -> ResolverStats {
        match self {
            ResolverStats::Resolving(info) => op(info),
            _ => self,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, ResolverStats::Success(_))
    }

    pub fn extract_info(self) -> ResolverInfo {
        match self {
            ResolverStats::Resolving(info) => info,
            ResolverStats::Error((_, info)) => info,
            _ => unreachable!(),
        }
    }
}

pub(crate) static MODULE: &str = "node_modules";

pub(crate) type RResult<T> = Result<T, ResolverError>;

impl Resolver {
    pub fn new(options: ResolverOptions) -> Self {
        let unsafe_cache = if let Some(external_unsafe_cache) = options.unsafe_cache.as_ref() {
            Some(external_unsafe_cache.clone())
        } else {
            Some(Arc::new(ResolverUnsafeCache::default()))
        };
        let safe_cache = ResolverSafeCache::default();
        let extensions: Vec<String> = options
            .extensions
            .into_iter()
            .map(|s| {
                if let Some(striped) = s.strip_prefix('.') {
                    striped.to_string()
                } else {
                    s
                }
            })
            .collect();
        let enforce_extension = if options.enforce_extension.is_none() {
            Some(extensions.iter().any(|ext| ext.is_empty()))
        } else {
            options.enforce_extension
        };
        let options = ResolverOptions {
            extensions,
            enforce_extension,
            ..options
        };
        Self {
            options,
            unsafe_cache,
            safe_cache,
            input_path: None,
            input_request: None,
            // dbg_map: Default::default(),
        }
    }

    pub fn resolve(&self, path: &Path, request: &str) -> RResult<ResolverResult> {
        let info = ResolverInfo::from(path.to_path_buf(), self.parse(request));

        let result = if let Some(tsconfig_location) = self.options.tsconfig.as_ref() {
            self._resolve_with_tsconfig(info, tsconfig_location)
        } else {
            self._resolve(info)
        };
        match result {
            ResolverStats::Success(result) => self.normalize_result(result),
            ResolverStats::Error((err_msg, _)) => Err(err_msg),
            _ => unreachable!(),
        }
    }

    #[tracing::instrument]
    fn _resolve(&self, info: ResolverInfo) -> ResolverStats {
        let resolve_err_msg = Self::raise_resolve_failed_message(&info);
        let stats = AliasPlugin::default()
            .apply(self, info)
            .and_then(|info| PreferRelativePlugin::default().apply(self, info))
            .and_then(|info| {
                let request = if info.request.kind.eq(&PathKind::Normal) {
                    info.path.join(MODULE).join(&*info.request.target)
                } else {
                    info.get_path()
                };
                let pkg_info_wrap = match self.load_pkg_file(&request) {
                    Ok(pkg_info_wrap) => pkg_info_wrap,
                    Err(error) => return ResolverStats::Error((error, info)),
                };
                ImportsFieldPlugin::new(&pkg_info_wrap)
                    .apply(self, info)
                    .and_then(|info| AliasFieldPlugin::new(&pkg_info_wrap).apply(self, info))
            })
            .and_then(|info| {
                if matches!(
                    info.request.kind,
                    PathKind::AbsolutePosix | PathKind::AbsoluteWin | PathKind::Relative
                ) {
                    self.resolve_as_file(info)
                        .and_then(|info| self.resolve_as_dir(info))
                } else {
                    self.resolve_as_modules(info)
                }
            });

        match stats {
            ResolverStats::Success(result) => ResolverStats::Success(result),
            ResolverStats::Error((err_msg, info)) => {
                let err_msg = if err_msg.eq(RAISE_RESOLVE_ERROR_TAG) {
                    resolve_err_msg
                } else {
                    err_msg
                };
                ResolverStats::Error((err_msg, info))
            }
            _ => unreachable!(),
        }
    }
}
