/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, Mutex};

use app_units::Au;
use fonts::{
    FallbackFontSelectionOptions, FontContext, FontDescriptor, FontFamilyDescriptor,
    FontSearchScope, fallback_font_families,
};
use net_traits::image_cache::FontResolver;
use resvg::usvg::{Font, FontFamily, FontStretch, FontStyle, fontdb};
use rustc_hash::FxHashMap;
use style::computed_values::font_optical_sizing::T as FontOpticalSizing;
use style::properties::longhands::font_variant_caps::computed_value::T as FontVariantCaps;
use style::values::computed::font::{
    FamilyName, FontFamilyNameSyntax, GenericFontFamily, SingleFontFamily,
};
use style::values::computed::{
    FontStyle as ServoFontStyle, FontSynthesis, FontWeight, FontWidth as ServoFontWidth,
};
use webrender_api::FontVariation;

/// Used to dynamically query fonts used in SVGs and insert them into the fontDB used when rasterizing.
#[derive(MallocSizeOf)]
pub struct SvgFontResolver {
    /// Cache for Font to the font data. The `fontdb::ID` of a face is only valid in the database it was loaded
    /// into, and every SVG tree gets its own copy of the database, so IDs cannot be cached across trees.
    #[ignore_malloc_size_of = "shared font data"]
    font_cache: Mutex<FxHashMap<Font, FontSource>>,
    #[ignore_malloc_size_of = "shared font data"]
    fallback_cache: Mutex<FxHashMap<char, Vec<FontSource>>>,
    #[conditional_malloc_size_of]
    context: Arc<FontContext>,
}

type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// The binary data of a font, and the index of the face in it.
#[derive(Clone)]
struct FontSource {
    data: FontData,
    index: u32,
}

impl FontSource {
    /// The ID of this face in `database`, loading the font into it when it is not there yet.
    fn id_in(&self, database: &mut Arc<fontdb::Database>) -> Option<fontdb::ID> {
        let loaded = database.faces().find(|face| {
            face.index == self.index &&
                matches!(&face.source, fontdb::Source::Binary(data) if Arc::ptr_eq(data, &self.data))
        });
        if let Some(face) = loaded {
            return Some(face.id);
        }
        let ids =
            Arc::make_mut(database).load_font_source(fontdb::Source::Binary(self.data.clone()));
        ids.get(self.index as usize).copied()
    }
}

impl SvgFontResolver {
    pub(crate) fn new(context: Arc<FontContext>) -> Self {
        Self {
            font_cache: Mutex::new(FxHashMap::default()),
            fallback_cache: Mutex::new(FxHashMap::default()),
            context,
        }
    }

    /// Look up the font in the font system and put it into the cache.
    fn find_source(&self, font: &Font) -> Option<FontSource> {
        let font_descriptor = font_to_fontdescriptor(font);

        for family in font.families() {
            let family_descriptor = FontFamilyDescriptor::new(
                fontfamily_to_singlefontfamily(family),
                FontSearchScope::Any,
            );

            let Some(font_template) = self
                .context
                .matching_templates(&font_descriptor, &family_descriptor)
                .into_iter()
                .next()
            else {
                log::debug!(
                    "Cannot find matching font_template from font {font:?}, font-descriptor {font_descriptor:?}, family_descriptor: {family_descriptor:?} for this family {family:?}"
                );
                continue;
            };

            let Some(font_ref) = self.context.font(font_template, &font_descriptor) else {
                continue;
            };

            let Ok(data_and_index) = font_ref.font_data_and_index() else {
                continue;
            };
            let source = FontSource {
                data: Arc::new(data_and_index.data.clone()),
                index: data_and_index.index,
            };
            self.font_cache
                .lock()
                .unwrap()
                .insert(font.clone(), source.clone());
            return Some(source);
        }

        None
    }
}

fn font_to_fontdescriptor(font: &Font) -> FontDescriptor {
    let style = match font.style() {
        FontStyle::Normal => ServoFontStyle::normal(),
        FontStyle::Italic => ServoFontStyle::ITALIC,
        FontStyle::Oblique => ServoFontStyle::OBLIQUE,
    };

    let width = match font.stretch() {
        FontStretch::UltraCondensed => ServoFontWidth::ULTRA_CONDENSED,
        FontStretch::ExtraCondensed => ServoFontWidth::EXTRA_CONDENSED,
        FontStretch::Condensed => ServoFontWidth::CONDENSED,
        FontStretch::SemiCondensed => ServoFontWidth::SEMI_CONDENSED,
        FontStretch::Normal => ServoFontWidth::NORMAL,
        FontStretch::SemiExpanded => ServoFontWidth::SEMI_EXPANDED,
        FontStretch::Expanded => ServoFontWidth::EXPANDED,
        FontStretch::ExtraExpanded => ServoFontWidth::EXTRA_EXPANDED,
        FontStretch::UltraExpanded => ServoFontWidth::ULTRA_EXPANDED,
    };

    let variation_settings = font
        .variations()
        .iter()
        .map(|variation| FontVariation {
            tag: u32::from_be_bytes(variation.tag),
            value: variation.value,
        })
        .collect();

    FontDescriptor {
        weight: FontWeight::from_float(font.weight() as f32),
        width,
        style,
        variant: FontVariantCaps::Normal,
        pt_size: Au::from_px(16),
        variation_settings,
        synthesis_weight: FontSynthesis::Auto,
        optical_sizing: FontOpticalSizing::Auto,
    }
}

fn fallback_descriptor() -> FontDescriptor {
    FontDescriptor {
        weight: FontWeight::normal(),
        width: ServoFontWidth::hundred(),
        style: ServoFontStyle::normal(),
        variant: FontVariantCaps::Normal,
        pt_size: Au::from_px(16),
        variation_settings: vec![],
        synthesis_weight: FontSynthesis::Auto,
        optical_sizing: FontOpticalSizing::Auto,
    }
}

fn fontfamily_to_singlefontfamily(family: &FontFamily) -> SingleFontFamily {
    match family {
        FontFamily::Serif => SingleFontFamily::Generic(GenericFontFamily::Serif),
        FontFamily::SansSerif => SingleFontFamily::Generic(GenericFontFamily::SansSerif),
        FontFamily::Cursive => SingleFontFamily::Generic(GenericFontFamily::Cursive),
        FontFamily::Fantasy => SingleFontFamily::Generic(GenericFontFamily::Fantasy),
        FontFamily::Monospace => SingleFontFamily::Generic(GenericFontFamily::Monospace),
        FontFamily::Named(name) => SingleFontFamily::FamilyName(FamilyName {
            name: name.as_str().into(),
            syntax: FontFamilyNameSyntax::Quoted,
        }),
    }
}

impl FontResolver for SvgFontResolver {
    fn resolve(&self, font: &Font, database: &mut Arc<fontdb::Database>) -> Option<fontdb::ID> {
        let cached = self.font_cache.lock().unwrap().get(font).cloned();
        let source = cached.or_else(|| self.find_source(font))?;
        source.id_in(database)
    }

    fn resolve_fallback(
        &self,
        character: char,
        excluded: &[fontdb::ID],
        database: &mut Arc<fontdb::Database>,
    ) -> Option<fontdb::ID> {
        let cached = self
            .fallback_cache
            .lock()
            .unwrap()
            .get(&character)
            .cloned()
            .unwrap_or_default();
        for source in cached {
            if let Some(id) = source.id_in(database) {
                if !excluded.contains(&id) {
                    return Some(id);
                }
            }
        }
        let fallback_options = FallbackFontSelectionOptions::new(
            character,
            None,
            icu_locale_core::subtags::Language::UNKNOWN,
        );
        for family in fallback_font_families(fallback_options) {
            let family = FontFamilyDescriptor::new(
                SingleFontFamily::FamilyName(FamilyName {
                    name: family.into(),
                    syntax: FontFamilyNameSyntax::Quoted,
                }),
                FontSearchScope::Any,
            );
            let fallback_descriptor = fallback_descriptor();

            let font_templates = self
                .context
                .matching_templates(&fallback_descriptor, &family);

            for font_template in font_templates {
                let Some(font_ref) = self.context.font(font_template, &fallback_descriptor) else {
                    continue;
                };
                if !font_ref.has_glyph_for(character) {
                    continue;
                }

                let Ok(data_and_index) = font_ref.font_data_and_index() else {
                    continue;
                };

                let source = FontSource {
                    data: Arc::new(data_and_index.data.clone()),
                    index: data_and_index.index,
                };
                let Some(id) = source.id_in(database) else {
                    continue;
                };

                if excluded.contains(&id) {
                    continue;
                }

                self.fallback_cache
                    .lock()
                    .unwrap()
                    .entry(character)
                    .or_default()
                    .push(source);
                return Some(id);
            }
        }
        None
    }
}
