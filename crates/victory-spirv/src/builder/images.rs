//! Image and sampler instructions: the console's texture and buffer access
//! path onto Vulkan's combined image samplers and storage images.

use crate::module::Id;
use crate::ops::Op;

use super::Builder;

/// The extra operands an image access carries, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operands<'a> {
    /// None at all.
    None,
    /// `mask` and the operands it announces.
    Some(u32, &'a [Id]),
}

impl Operands<'_> {
    fn words(self) -> Vec<u32> {
        match self {
            Operands::None => Vec::new(),
            Operands::Some(mask, ids) => {
                let mut out = vec![mask];
                out.extend(ids.iter().map(|i| i.0));
                out
            }
        }
    }
}

impl Builder {
    /// `OpSampledImage`: an image and a sampler, for a sampling operation.
    pub fn sampled_image(&mut self, ty: Id, image: Id, sampler: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::SampledImage, Some(ty), Some(id), &[image.0, sampler.0]);
        id
    }

    /// `OpImage`: the image out of a sampled image.
    pub fn image_from_sampled(&mut self, ty: Id, sampled: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::Image, Some(ty), Some(id), &[sampled.0]);
        id
    }

    /// `OpImageSampleImplicitLod`.
    pub fn image_sample_implicit_lod(
        &mut self,
        ty: Id,
        sampled: Id,
        coordinate: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![sampled.0, coordinate.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageSampleImplicitLod, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageSampleExplicitLod`, whose operands must announce a LOD or a
    /// gradient.
    pub fn image_sample_explicit_lod(
        &mut self,
        ty: Id,
        sampled: Id,
        coordinate: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![sampled.0, coordinate.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageSampleExplicitLod, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageSampleDrefImplicitLod`: a comparison against `dref`.
    pub fn image_sample_dref_implicit_lod(
        &mut self,
        ty: Id,
        sampled: Id,
        coordinate: Id,
        dref: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![sampled.0, coordinate.0, dref.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageSampleDrefImplicitLod, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageSampleDrefExplicitLod`.
    pub fn image_sample_dref_explicit_lod(
        &mut self,
        ty: Id,
        sampled: Id,
        coordinate: Id,
        dref: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![sampled.0, coordinate.0, dref.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageSampleDrefExplicitLod, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageFetch`, which reads a texel without a sampler.
    pub fn image_fetch(
        &mut self,
        ty: Id,
        image: Id,
        coordinate: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![image.0, coordinate.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageFetch, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageGather`: four texels around `coordinate`.
    pub fn image_gather(
        &mut self,
        ty: Id,
        sampled: Id,
        coordinate: Id,
        component: Id,
        operands: Operands<'_>,
    ) -> Id {
        let mut words = vec![sampled.0, coordinate.0, component.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module
            .op(Op::ImageGather, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageRead` of `ty` from a storage image.
    pub fn image_read(&mut self, ty: Id, image: Id, coordinate: Id, operands: Operands<'_>) -> Id {
        let mut words = vec![image.0, coordinate.0];
        words.extend(operands.words());
        let id = self.module.id();
        self.module.op(Op::ImageRead, Some(ty), Some(id), &words);
        id
    }

    /// `OpImageWrite` of `texel` into a storage image.
    pub fn image_write(&mut self, image: Id, coordinate: Id, texel: Id) {
        self.module
            .op(Op::ImageWrite, None, None, &[image.0, coordinate.0, texel.0]);
    }

    /// `OpImageQuerySizeLod`.
    pub fn image_query_size_lod(&mut self, ty: Id, image: Id, lod: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ImageQuerySizeLod, Some(ty), Some(id), &[image.0, lod.0]);
        id
    }

    /// `OpImageQuerySize`.
    pub fn image_query_size(&mut self, ty: Id, image: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ImageQuerySize, Some(ty), Some(id), &[image.0]);
        id
    }

    /// `OpImageQueryLod`.
    pub fn image_query_lod(&mut self, ty: Id, sampled: Id, coordinate: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::ImageQueryLod,
            Some(ty),
            Some(id),
            &[sampled.0, coordinate.0],
        );
        id
    }

    /// `OpImageQueryLevels`.
    pub fn image_query_levels(&mut self, ty: Id, image: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ImageQueryLevels, Some(ty), Some(id), &[image.0]);
        id
    }

    /// `OpImageQuerySamples`.
    pub fn image_query_samples(&mut self, ty: Id, image: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ImageQuerySamples, Some(ty), Some(id), &[image.0]);
        id
    }

    /// `OpImageTexelPointer`: the atomic access point of one texel.
    pub fn image_texel_pointer(&mut self, ty: Id, image: Id, coordinate: Id, sample: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::ImageTexelPointer,
            Some(ty),
            Some(id),
            &[image.0, coordinate.0, sample.0],
        );
        id
    }
}
