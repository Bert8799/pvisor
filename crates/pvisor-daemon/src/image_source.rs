use crate::api::{E2bTemplate, NativeTemplate, PreparedImageSource};
use std::{collections::BTreeMap, path::PathBuf};

pub(crate) struct ImageConfiguration<'a> {
    pub(crate) rafs_node: Option<&'a pvisor::rafs::RafsNodeRootfs>,
    pub(crate) rootfs: &'a PathBuf,
    pub(crate) entrypoint: &'a Vec<String>,
    pub(crate) cmd: &'a Vec<String>,
    pub(crate) env: &'a BTreeMap<String, String>,
    pub(crate) library_dir: &'a Option<PathBuf>,
    pub(crate) e2b: &'a Option<E2bTemplate>,
    pub(crate) native_opensandbox: bool,
    pub(crate) native_e2b: bool,
    pub(crate) native_guest: bool,
    pub(crate) cwd: &'a Option<PathBuf>,
    pub(crate) template: Option<&'a NativeTemplate>,
}

impl PreparedImageSource {
    pub(crate) fn cold_image(
        &self,
        restoring: bool,
    ) -> anyhow::Result<Option<&pvisor::docker::Image>> {
        match (self, restoring) {
            (_, true) => Ok(None),
            (Self::RafsNode(_), false) => Ok(None),
            (Self::Pooled(image), false) => Ok(Some(&image.pooled_image)),
            (Self::DirectoryTemplate(_), false) => {
                anyhow::bail!("directory template requires a restored generation")
            }
        }
    }

    pub(crate) fn config(&self) -> ImageConfiguration<'_> {
        macro_rules! configuration {
            ($image:ident, $template:expr, $root:expr, $node:expr) => {
                ImageConfiguration {
                    rootfs: $root,
                    rafs_node: $node,
                    entrypoint: &$image.entrypoint,
                    cmd: &$image.cmd,
                    env: &$image.env,
                    library_dir: &$image.library_dir,
                    e2b: &$image.e2b,
                    native_opensandbox: $image.native_opensandbox,
                    native_e2b: $image.native_e2b,
                    native_guest: $image.native_guest,
                    cwd: &$image.cwd,
                    template: $template,
                }
            };
        }
        match self {
            Self::Pooled(image) => {
                configuration!(image, image.template.as_ref(), &image.rootfs, None)
            }
            Self::RafsNode(image) => configuration!(
                image,
                image.template.as_ref(),
                &image.rafs_node.store,
                Some(&image.rafs_node)
            ),
            Self::DirectoryTemplate(image) => {
                configuration!(image, Some(&image.template), &image.rootfs, None)
            }
        }
    }
}
