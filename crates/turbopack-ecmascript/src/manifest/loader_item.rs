use std::io::Write as _;

use anyhow::{anyhow, Result};
use indoc::writedoc;
use turbo_tasks::{TryJoinIterExt, Value, Vc};
use turbopack_core::{
    chunk::{
        availability_info::AvailabilityInfo, Chunk, ChunkData, ChunkItem, ChunkItemExt,
        ChunkableModule, ChunkingContext, ChunksData,
    },
    ident::AssetIdent,
    module::Module,
    reference::{ModuleReference, ModuleReferences, SingleOutputAssetReference},
};

use super::chunk_asset::ManifestChunkAsset;
use crate::{
    chunk::{
        data::EcmascriptChunkData, EcmascriptChunk, EcmascriptChunkItem,
        EcmascriptChunkItemContent, EcmascriptChunkPlaceable, EcmascriptChunkingContext,
    },
    utils::StringifyJs,
};

#[turbo_tasks::function]
fn modifier() -> Vc<String> {
    Vc::cell("loader".to_string())
}

/// The manifest loader item is shipped in the same chunk that uses the dynamic
/// `import()` expression. Its responsibility is to load the manifest chunk from
/// the server. The dynamic import has been rewritten to import this manifest
/// loader item, which will load the manifest chunk from the server, which
/// will load all the chunks needed by the dynamic import. Finally, we'll be
/// able to import the module we're trying to dynamically import.
///
/// Splitting the dynamic import into a quickly generate-able manifest loader
/// item and a slow-to-generate manifest chunk allows for faster incremental
/// compilation. The traversal won't be performed until the dynamic import is
/// actually reached, instead of eagerly as part of the chunk that the dynamic
/// import appears in.
#[turbo_tasks::value]
pub struct ManifestLoaderItem {
    manifest: Vc<ManifestChunkAsset>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
}

#[turbo_tasks::value_impl]
impl ManifestLoaderItem {
    #[turbo_tasks::function]
    pub fn new(
        manifest: Vc<ManifestChunkAsset>,
        chunking_context: Vc<Box<dyn ChunkingContext>>,
    ) -> Vc<Self> {
        Self::cell(ManifestLoaderItem {
            manifest,
            chunking_context,
        })
    }

    #[turbo_tasks::function]
    pub async fn chunks_data(self: Vc<Self>) -> Result<Vc<ChunksData>> {
        let this = self.await?;
        let chunks = this.manifest.manifest_chunks();
        Ok(ChunkData::from_assets(
            this.chunking_context.output_root(),
            chunks,
        ))
    }
}

#[turbo_tasks::function]
fn manifest_loader_chunk_reference_description() -> Vc<String> {
    Vc::cell("manifest loader chunk".to_string())
}

#[turbo_tasks::function]
fn chunk_data_reference_description() -> Vc<String> {
    Vc::cell("chunk data reference".to_string())
}

#[turbo_tasks::value_impl]
impl ChunkItem for ManifestLoaderItem {
    #[turbo_tasks::function]
    fn asset_ident(&self) -> Vc<AssetIdent> {
        self.manifest.ident().with_modifier(modifier())
    }

    #[turbo_tasks::function]
    async fn references(self: Vc<Self>) -> Result<Vc<ModuleReferences>> {
        let this = self.await?;

        let chunks = this.manifest.manifest_chunks();

        let mut references: Vec<Vc<Box<dyn ModuleReference>>> = chunks
            .await?
            .iter()
            .map(|&chunk| {
                Vc::upcast(SingleOutputAssetReference::new(
                    chunk,
                    manifest_loader_chunk_reference_description(),
                ))
            })
            .collect();

        for chunk_data in &*self.chunks_data().await? {
            references.extend(chunk_data.references().await?.iter().map(|&output_asset| {
                Vc::upcast(SingleOutputAssetReference::new(
                    output_asset,
                    chunk_data_reference_description(),
                ))
            }));
        }

        Ok(Vc::cell(references))
    }

    #[turbo_tasks::function]
    async fn chunking_context(&self) -> Result<Vc<Box<dyn ChunkingContext>>> {
        Ok(Vc::upcast(self.chunking_context))
    }

    #[turbo_tasks::function]
    fn as_chunk(&self, availability_info: Value<AvailabilityInfo>) -> Vc<Box<dyn Chunk>> {
        Vc::upcast(EcmascriptChunk::new(
            self.chunking_context,
            Vc::upcast(self.manifest),
            availability_info,
        ))
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkItem for ManifestLoaderItem {
    #[turbo_tasks::function]
    async fn chunking_context(&self) -> Result<Vc<Box<dyn EcmascriptChunkingContext>>> {
        Ok(self.manifest.await?.chunking_context)
    }

    #[turbo_tasks::function]
    async fn content(self: Vc<Self>) -> Result<Vc<EcmascriptChunkItemContent>> {
        let this = &*self.await?;
        let mut code = Vec::new();

        let manifest = this.manifest.await?;

        // We need several items in order for a dynamic import to fully load. First, we
        // need the chunk path of the manifest chunk, relative from the output root. The
        // chunk is a servable file, which will contain the manifest chunk item, which
        // will perform the actual chunk traversal and generate load statements.
        let chunks_server_data = &*self.chunks_data().await?.iter().try_join().await?;

        // We also need the manifest chunk item's id, which points to a CJS module that
        // exports a promise for all of the necessary chunk loads.
        let item_id = &*this
            .manifest
            .as_chunk_item(Vc::upcast(manifest.chunking_context))
            .id()
            .await?;

        // Finally, we need the id of the module that we're actually trying to
        // dynamically import.
        let placeable =
            Vc::try_resolve_downcast::<Box<dyn EcmascriptChunkPlaceable>>(manifest.asset)
                .await?
                .ok_or_else(|| anyhow!("asset is not placeable in ecmascript chunk"))?;
        let dynamic_id = &*placeable
            .as_chunk_item(Vc::upcast(manifest.chunking_context))
            .id()
            .await?;

        // This is the code that will be executed when the dynamic import is reached.
        // It will load the manifest chunk, which will load all the chunks needed by
        // the dynamic import, and finally we'll be able to import the module we're
        // trying to dynamically import.
        // This is similar to what happens when the first evaluated chunk is executed
        // on first page load, but it's happening on-demand instead of eagerly.
        writedoc!(
            code,
            r#"
                __turbopack_export_value__((__turbopack_import__) => {{
                    return Promise.all({chunks_server_data}.map((chunk) => __turbopack_load__(chunk))).then(() => {{
                        return __turbopack_require__({item_id});
                    }}).then((chunks) => {{
                        return Promise.all(chunks.map((chunk) => __turbopack_load__(chunk)));
                    }}).then(() => {{
                        return __turbopack_import__({dynamic_id});
                    }});
                }});
            "#,
            chunks_server_data = StringifyJs(
                &chunks_server_data
                    .iter()
                    .map(|chunk_data| EcmascriptChunkData::new(chunk_data))
                    .collect::<Vec<_>>()
            ),
            item_id = StringifyJs(item_id),
            dynamic_id = StringifyJs(dynamic_id),
        )?;

        Ok(EcmascriptChunkItemContent {
            inner_code: code.into(),
            ..Default::default()
        }
        .into())
    }
}