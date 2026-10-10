//! PDF attachment helpers: page size lookup and page→Form XObject import.
//! Background pages are embedded as vector Form XObjects in PDF output
//! (no rasterization).

use std::collections::HashMap;

use lopdf::{Dictionary, Document as PdfDoc, Object, ObjectId, Stream, dictionary};

/// Width × height of a 1-based page in PDF points (viewer orientation).
pub(crate) fn page_size(pdf: &[u8], page_1based: u64) -> Option<(f32, f32)> {
    let doc = PdfDoc::load_mem(pdf).ok()?;
    let pages = doc.get_pages();
    let page_id = *pages.get(&(page_1based as u32))?;
    let bbox = page_bbox_of(&doc, page_id)?;
    let rotate = page_rotate(&doc, page_id);
    let (w, h) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
    if !(w > 1.0 && h > 1.0) {
        return None;
    }
    if rotate == 90 || rotate == 270 {
        Some((h, w))
    } else {
        Some((w, h))
    }
}

fn resolve<'a>(doc: &'a PdfDoc, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

fn page_bbox_of(doc: &PdfDoc, page_id: ObjectId) -> Option<[f32; 4]> {
    // CropBox/MediaBox may live on the page or be inherited from /Pages ancestors.
    let mut id = page_id;
    for _ in 0..32 {
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        if let Some(b) = box_of(doc, dict, b"CropBox").or_else(|| box_of(doc, dict, b"MediaBox")) {
            let (w, h) = (b[2] - b[0], b[3] - b[1]);
            if w > 1.0 && h > 1.0 {
                return Some(b);
            }
        }
        let parent = match dict.get(b"Parent") {
            Ok(Object::Reference(p)) => *p,
            _ => break,
        };
        if parent == id {
            break;
        }
        id = parent;
    }
    Some([0.0, 0.0, 612.0, 792.0])
}

fn box_of(doc: &PdfDoc, dict: &Dictionary, key: &[u8]) -> Option<[f32; 4]> {
    let obj = resolve(doc, dict.get(key).ok()?)?;
    let arr = obj.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let n = |o: &Object| -> Option<f32> {
        match o {
            Object::Integer(i) => Some(*i as f32),
            Object::Real(r) => Some(*r),
            Object::Reference(id) => {
                let r = doc.get_object(*id).ok()?;
                match r {
                    Object::Integer(i) => Some(*i as f32),
                    Object::Real(v) => Some(*v),
                    _ => None,
                }
            }
            _ => None,
        }
    };
    Some([n(&arr[0])?, n(&arr[1])?, n(&arr[2])?, n(&arr[3])?])
}

fn page_rotate(doc: &PdfDoc, page_id: ObjectId) -> i64 {
    doc.get_dictionary(page_id)
        .ok()
        .and_then(|p| p.get(b"Rotate").ok())
        .and_then(|o| resolve(doc, o))
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360)
}

/// Import one page of `src` into `dst` as a Form XObject.
/// Returns the new object id and the form BBox `[x0,y0,x1,y1]` in the
/// orientation used for placement (rotation baked into `/Matrix` when needed).
pub(crate) fn import_page_as_form(
    src: &PdfDoc,
    page_1based: u32,
    dst: &mut PdfDoc,
) -> Option<(ObjectId, [f32; 4])> {
    let pages = src.get_pages();
    let page_id = *pages.get(&page_1based)?;
    let bbox = page_bbox_of(src, page_id)?;
    let content = src.get_page_content(page_id);
    if content.is_empty() {
        // Still embed an empty form so placement stays consistent.
    }
    let rotate = page_rotate(src, page_id);

    let mut id_map: HashMap<ObjectId, ObjectId> = HashMap::new();
    let resources = match src.get_dictionary(page_id).ok().and_then(|p| p.get(b"Resources").ok()) {
        Some(Object::Reference(id)) => Object::Reference(copy_ref(src, *id, dst, &mut id_map)?),
        Some(Object::Dictionary(dict)) => {
            copy_object(src, Object::Dictionary(dict.clone()), dst, &mut id_map)?
        }
        Some(other) => copy_object(src, other.clone(), dst, &mut id_map)?,
        None => Object::Dictionary(Dictionary::new()),
    };

    let (x0, y0, x1, y1) = (bbox[0], bbox[1], bbox[2], bbox[3]);
    let (bw, bh) = (x1 - x0, y1 - y0);

    // Form BBox is always origin-aligned; page origin offsets go into Matrix.
    let (form_w, form_h, matrix) = match rotate {
        90 => (
            bh,
            bw,
            vec![0.0, 1.0, -1.0, 0.0, y1, -x0],
        ),
        180 => (
            bw,
            bh,
            vec![-1.0, 0.0, 0.0, -1.0, x1, y1],
        ),
        270 => (
            bh,
            bw,
            vec![0.0, -1.0, 1.0, 0.0, -y0, x1],
        ),
        _ => (bw, bh, vec![1.0, 0.0, 0.0, 1.0, -x0, -y0]),
    };

    let dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "FormType" => 1,
        "BBox" => Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(form_w),
            Object::Real(form_h),
        ]),
        "Matrix" => Object::Array(matrix.into_iter().map(Object::Real).collect()),
        "Resources" => resources,
    };

    let mut stream = Stream::new(dict, content);
    let _ = stream.compress();
    let id = dst.add_object(Object::Stream(stream));
    Some((id, [0.0, 0.0, form_w, form_h]))
}

fn copy_ref(
    src: &PdfDoc,
    src_id: ObjectId,
    dst: &mut PdfDoc,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> Option<ObjectId> {
    if let Some(&id) = map.get(&src_id) {
        return Some(id);
    }
    // Reserve an id first so cycles (rare in Resources) terminate.
    let new_id = dst.new_object_id();
    map.insert(src_id, new_id);
    let obj = src.get_object(src_id).ok()?.clone();
    let copied = copy_object(src, obj, dst, map)?;
    dst.set_object(new_id, copied);
    Some(new_id)
}

fn copy_object(
    src: &PdfDoc,
    obj: Object,
    dst: &mut PdfDoc,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> Option<Object> {
    match obj {
        Object::Reference(id) => Some(Object::Reference(copy_ref(src, id, dst, map)?)),
        Object::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(copy_object(src, item, dst, map)?);
            }
            Some(Object::Array(out))
        }
        Object::Dictionary(dict) => Some(Object::Dictionary(copy_dict(src, dict, dst, map)?)),
        Object::Stream(stream) => {
            let mut new_dict = copy_dict(src, stream.dict, dst, map)?;
            new_dict.remove(b"Length");
            let mut new_stream = Stream::new(new_dict, stream.content);
            new_stream.allows_compression = stream.allows_compression;
            Some(Object::Stream(new_stream))
        }
        other => Some(other),
    }
}

fn copy_dict(
    src: &PdfDoc,
    dict: Dictionary,
    dst: &mut PdfDoc,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> Option<Dictionary> {
    let mut out = Dictionary::new();
    for (k, v) in dict.into_iter() {
        // Drop page-tree back-references if a resource dict ever carries them.
        if k == b"Parent" || k == b"P" {
            continue;
        }
        out.set(k, copy_object(src, v, dst, map)?);
    }
    Some(out)
}

/// Load a PDF attachment for repeated page imports.
pub(crate) fn load_pdf(bytes: &[u8]) -> Option<PdfDoc> {
    PdfDoc::load_mem(bytes).ok()
}
