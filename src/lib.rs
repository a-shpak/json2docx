mod error;
mod numbering;
mod types;
mod utils;

use std::io::Cursor;
use std::pin::Pin;

use base64::{engine::general_purpose, Engine};

use docx_rs::{
    AbstractNumbering, AlignmentType, BreakType, Docx, Hyperlink, HyperlinkType, IndentLevel,
    LineSpacing, Numbering, NumberingId, Paragraph, ParagraphChild, ParagraphProperty, Pic, Run,
    RunFonts, RunProperty, Shading, ShdType, VertAlignType,
};
use error::DocError;
use js_sys::Uint8Array;
use numbering::{NumberingData, NumberingType};
use types::{Chunk, ChunkType, MetaProps, Properties};
use wasm_bindgen;
use wasm_bindgen::prelude::*;

use gloo_utils::format::JsValueSerdeExt;

use wasm_bindgen_futures::{self, JsFuture};
use wasm_bindgen_test::console_log;
use web_sys::{Request, RequestInit, RequestMode, Response};

#[wasm_bindgen]
#[derive(Default)]
pub struct DocxDocument {
    chunks: Vec<Chunk>,
    stack: Vec<ChunkType>,
    it: usize,
    it_start: bool,
    numberings: Vec<NumberingData>,

    futures: Vec<(
        String,
        Pin<Box<dyn std::future::Future<Output = Result<JsValue, JsValue>>>>,
    )>,
}

use wasm_bindgen::JsValue; // Assuming you're working with WebAssembly here

#[wasm_bindgen]
impl DocxDocument {
    pub fn new() -> DocxDocument {
        Default::default()
    }

    pub async fn from_js_chunks(&mut self, raw: &JsValue) -> Result<JsValue, JsValue> {
        utils::set_panic_hook();
        let chunks: Vec<Chunk> = raw.into_serde().unwrap();
        let res = self.from_chunks(chunks).await;

        let val = JsValue::from_serde(&res).unwrap();

        Ok(val)
    }

    async fn from_chunks(&mut self, chunks: Vec<Chunk>) -> Vec<u8> {
        self.chunks = chunks;

        let docx = self.build().await.unwrap();

        let buf: Vec<u8> = vec![];
        let w: Cursor<Vec<u8>> = Cursor::new(buf);

        let res = docx.build().pack(w).unwrap();

        let bytes = res.get_ref().to_owned();

        bytes
    }

    async fn build(&mut self) -> Result<Docx, DocError> {
        let mut doc: Docx = docx_rs::Docx::new()
            .default_size(utils::px_to_docx_points(utils::DEFAULT_SZ_PX as i32) as usize);

        while self.next().is_some() {
            let chunk = self.curr().unwrap();

            match chunk.chunk_type {
                ChunkType::Paragraph => {
                    doc = doc.add_paragraph(self.parse_block(&chunk)?);
                }
                ChunkType::Ol | ChunkType::Ul => {
                    let list =
                        self.parse_numbering(0, NumberingType::from_chunk_type(chunk.chunk_type)?)?;

                    for p in list.iter() {
                        doc = doc.add_paragraph(p.to_owned());
                    }
                }
                ChunkType::Break => doc = doc.add_paragraph(Paragraph::new()),
                _ => continue,
            }
        }

        if !self.stack.is_empty() {
            return Err(DocError::new("some block statements are not closed"));
        }

        doc = self.build_numbering(doc);

        for (id, f) in self.futures.iter_mut() {
            let res = f.await;

            match res {
                Ok(v) => {
                    let arr = Uint8Array::new(&v);
                    let b = arr.to_vec();
                    doc = doc.add_defer_image(id.to_owned(), b);
                }
                Err(v) => console_log!("Error: {:?}", v),
            }
        }

        Ok(doc)
    }

    fn parse_block(&mut self, block_chunk: &Chunk) -> Result<Paragraph, DocError> {
        let mut para = Paragraph::new();

        let (children, max_sz) = self.parse_block_content(block_chunk, None)?;
        para.children = children;

        if let Some(p) = &block_chunk.props {
            para.property = self.parse_block_props(p, max_sz)?;
        }

        Ok(para)
    }

    fn parse_numbering(
        &mut self,
        level: usize,
        num_type: NumberingType,
    ) -> Result<Vec<Paragraph>, DocError> {
        let mut buf: Vec<Paragraph> = vec![];

        self.stack.push(self.curr().unwrap().chunk_type);

        let num_id = self.add_numbering(num_type);

        while self.next().is_some() {
            let c = self.curr().unwrap();

            match c.chunk_type {
                ChunkType::Ol => {
                    buf.append(&mut self.parse_numbering(level + 1, NumberingType::Decimal)?);
                }
                ChunkType::Ul => {
                    buf.append(&mut self.parse_numbering(level + 1, NumberingType::Bullet)?);
                }
                ChunkType::Li => {
                    let mut para = self.parse_block(&c)?;
                    para = para.numbering(NumberingId::new(num_id), IndentLevel::new(level));
                    buf.push(para);
                }
                ChunkType::End => {
                    self.stack_pop()?;
                    return Ok(buf);
                }
                _ => (),
            }
        }

        Err(DocError::new("unexpected end of statement"))
    }

    fn parse_block_content(
        &mut self,
        block_chunk: &Chunk,
        meta: Option<MetaProps>,
    ) -> Result<(Vec<ParagraphChild>, usize), DocError> {
        self.stack.push(block_chunk.chunk_type);

        let mut children: Vec<ParagraphChild> = vec![];
        let mut max_font_size: usize = 0;

        while self.next().is_some() {
            let c = self.curr().unwrap();

            match c.chunk_type {
                ChunkType::Text => {
                    let run = &self.parse_text(&c, meta)?;
                    let child = ParagraphChild::Run(Box::new(run.to_owned()));
                    children.push(child);

                    if let Some(props) = &c.props {
                        if let Some(fs) = &props.font_size {
                            if fs.get_val() as usize > max_font_size {
                                max_font_size = fs.get_val() as usize;
                            }
                        }
                    }
                }
                ChunkType::SubScript | ChunkType::SuperScript => {
                    let meta: MetaProps = MetaProps {
                        subscript: c.chunk_type == ChunkType::SubScript,
                        superscript: c.chunk_type == ChunkType::SuperScript,
                        ..Default::default()
                    };
                    let (sub_children, _) = self.parse_block_content(&c, Some(meta))?;

                    children.extend(sub_children);
                }
                ChunkType::Newline => {
                    let run = Run::new().add_break(BreakType::TextWrapping);
                    let child = ParagraphChild::Run(Box::new(run));
                    children.push(child);
                }
                ChunkType::Image => {
                    let pic = self.parse_pic(&c)?;
                    let run = Run::new().add_image(pic);
                    let child = ParagraphChild::Run(Box::new(run));
                    children.push(child);
                }
                ChunkType::Link => {
                    let mut hp = Hyperlink::new(
                        c.props.as_ref().unwrap().url.to_owned().unwrap(),
                        HyperlinkType::External,
                    );

                    let (link_children, max_sz) = self.parse_block_content(&c, None)?;
                    hp.children = link_children;

                    let child = ParagraphChild::Hyperlink(hp);
                    children.push(child);

                    if max_sz > max_font_size {
                        max_font_size = max_sz;
                    }
                }
                ChunkType::End => {
                    self.stack_pop()?;
                    return Ok((children, max_font_size));
                }
                _ => (),
            }
        }

        Err(DocError::new("unexpected end of statement"))
    }

    fn parse_pic(&mut self, chunk: &Chunk) -> Result<Pic, DocError> {
        let props = chunk.props.as_ref().unwrap();

        let w_px = props.width.as_ref().unwrap().get_val();
        let w_emu = utils::px_to_emu(w_px) as u32;
        let h_px = props.height.as_ref().unwrap().get_val();
        let h_emu = utils::px_to_emu(h_px) as u32;

        let mut pic = Pic::new(&vec![]).size(w_emu, h_emu);

        let buf = self.parse_pic_source(pic.id.to_owned(), chunk)?;
        pic = pic.buf(buf);

        Ok(pic)
    }

    fn parse_pic_source(&mut self, id: String, chunk: &Chunk) -> Result<Vec<u8>, DocError> {
        let url = chunk.props.as_ref().unwrap().url.to_owned().unwrap();

        if url.is_empty() {
            return Ok(vec![]);
        }

        if url.starts_with("http") {
            let res = download(url.to_owned());

            self.futures.push((id, Box::pin(res)));

            Ok(vec![])
        } else {
            // try convert from base64
            let res = general_purpose::STANDARD.decode(&url);
            match res {
                Ok(bytes) => return Ok(bytes),
                Err(e) => return Err(DocError::new(&e.to_string())),
            };
        }
    }

    fn parse_text(&self, chunk: &Chunk, meta: Option<MetaProps>) -> Result<Run, DocError> {
        let mut run = Run::new().add_text(chunk.text.to_owned().unwrap());
        if let Some(p) = &chunk.props {
            run.run_property = self.parse_run_props(p)?;
        }

        if let Some(m) = &meta {
            if m.subscript {
                run.run_property = run.run_property.vert_align(VertAlignType::SubScript);
            } else if m.superscript {
                run.run_property = run.run_property.vert_align(VertAlignType::SuperScript);
            }
        }

        Ok(run)
    }

    fn parse_block_props(
        &self,
        props: &Properties,
        max_sz: usize,
    ) -> Result<ParagraphProperty, DocError> {
        let mut para_props = ParagraphProperty::new();

        if let Some(align) = &props.align {
            let res = <AlignmentType as std::str::FromStr>::from_str(&align);
            match res {
                Ok(v) => para_props = para_props.align(v),
                Err(_) => return Err(DocError::new(&format!("unknown alignment type: {}", align))),
            };
        }
        if let Some(indent) = &props.indent {
            let left_emu = utils::px_to_indent(indent.get_val());
            para_props = para_props.indent(Some(left_emu), None, None, None);
        }
        if let Some(v) = &props.line_height {
            let line_height = match v.parse::<f32>() {
                Ok(v) => v,
                Err(_) => return Err(DocError::new(&format!("unbale to parse string: {}", v))),
            };

            let sz = if max_sz == 0 {
                utils::DEFAULT_SZ_PX
            } else {
                max_sz
            } as f32;

            let spacing_px = (((sz * line_height) - sz) / 2.0) as i32;
            let spacing = utils::px_to_indent(spacing_px) as u32;

            para_props = para_props.line_spacing(LineSpacing::new().after(spacing).before(spacing));
        }

        Ok(para_props)
    }

    fn parse_run_props(&self, props: &Properties) -> Result<RunProperty, DocError> {
        let mut run_props = RunProperty::new();
        if let Some(bold) = props.bold {
            if bold {
                run_props = run_props.bold();
            }
        }
        if let Some(strike) = props.strike {
            if strike {
                run_props = run_props.strike();
            }
        }
        if let Some(italic) = props.italic {
            if italic {
                run_props = run_props.italic();
            }
        }
        if let Some(underline) = props.underline {
            if underline {
                run_props = run_props.underline("single");
            }
        }
        if let Some(color) = &props.color {
            run_props = run_props.color(color);
        }
        if let Some(sz) = &props.font_size {
            let sz_pt = utils::px_to_docx_points(sz.get_val()) as usize;
            run_props = run_props.size(sz_pt);
        }
        if let Some(fam) = &props.font_family {
            run_props = run_props.fonts(RunFonts::new().ascii(fam));
        }
        if let Some(b) = &props.background {
            run_props = run_props.shading(Shading::new().shd_type(ShdType::Clear).fill(b));
        }

        Ok(run_props)
    }

    fn next(&mut self) -> Option<Chunk> {
        if self.it_start {
            self.it += 1;
        } else {
            self.it_start = true;
        }
        self.curr()
    }

    fn curr(&self) -> Option<Chunk> {
        if self.it >= self.chunks.len() {
            return None;
        }
        let chunk = &self.chunks[self.it];
        Some(chunk.clone())
    }

    fn stack_pop(&mut self) -> Result<(), DocError> {
        if self.stack.is_empty() {
            return Err(DocError::new("unexpected 'end' chunk"));
        }
        self.stack.pop();
        Ok(())
    }

    fn add_numbering(&mut self, t: NumberingType) -> usize {
        let id = self.numberings.len() + 2; // id=1 is preserved id
        self.numberings.push(NumberingData::new(id, t));
        id
    }

    fn build_numbering(&self, mut docx: Docx) -> Docx {
        for num in &self.numberings {
            let mut n = AbstractNumbering::new(num.get_id());
            for i in 0..9 {
                n = n.add_level(numbering::numbering_level(i, num.get_type()))
            }

            docx = docx
                .add_abstract_numbering(n)
                .add_numbering(Numbering::new(num.get_id(), num.get_id()));
        }

        docx
    }
}

pub async fn download(url: String) -> Result<JsValue, JsValue> {
    let mut opts = RequestInit::new();
    opts.method("GET");
    opts.mode(RequestMode::Cors);

    let request = Request::new_with_str_and_init(&url, &opts)?;

    request.headers().set("Accept", "image/*")?;

    let window = web_sys::window().unwrap();

    let resp_value = JsFuture::from(window.fetch_with_request(&request)).await?;

    // `resp_value` is a `Response` object.
    assert!(resp_value.is_instance_of::<Response>());
    let resp: Response = resp_value.dyn_into().unwrap();

    // Convert this other `Promise` into a rust `Future`.
    let blob = JsFuture::from(resp.array_buffer()?).await?;
    Ok(blob)
}

#[cfg(test)]
pub mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use crate::{
        types::{Chunk, ChunkType, Properties, Px},
        DocxDocument,
    };
    use std::io::Write;

    fn text(text: String, props: Option<Properties>) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Text,
            text: Some(text.to_owned()),
            props: props,
        }
    }
    fn para(props: Option<Properties>) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Paragraph,
            text: None,
            props: props,
        }
    }
    fn ol(props: Option<Properties>) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Ol,
            text: None,
            props: props,
        }
    }
    fn ul(props: Option<Properties>) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Ul,
            text: None,
            props: props,
        }
    }
    fn li(props: Option<Properties>) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Li,
            text: None,
            props: props,
        }
    }
    fn end() -> Chunk {
        Chunk {
            chunk_type: ChunkType::End,
            text: None,
            props: Default::default(),
        }
    }
    fn hyperlink(url: String) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Link,
            text: None,
            props: Some(Properties {
                url: Some(url),
                ..Default::default()
            }),
        }
    }
    fn image(url: &String, w: usize, h: usize) -> Chunk {
        Chunk {
            chunk_type: ChunkType::Image,
            props: Some(Properties {
                url: Some(url.to_owned()),
                width: Some(Px::new(w as i32)),
                height: Some(Px::new(h as i32)),
                ..Default::default()
            }),
            text: None,
        }
    }
    fn sub() -> Chunk {
        Chunk {
            chunk_type: ChunkType::SubScript,
            props: None,
            text: None,
        }
    }
    fn spr() -> Chunk {
        Chunk {
            chunk_type: ChunkType::SuperScript,
            props: None,
            text: None,
        }
    }
    fn br() -> Chunk {
        Chunk {
            chunk_type: ChunkType::Break,
            text: None,
            props: Default::default(),
        }
    }
    fn nl() -> Chunk {
        Chunk {
            chunk_type: ChunkType::Newline,
            text: None,
            props: Default::default(),
        }
    }

    fn save_docx(data: Vec<u8>, path: String) {
        let p = std::path::Path::new(&path);
        let mut file = std::fs::File::create(&p).unwrap();
        file.write_all(&data).unwrap();
    }

    #[actix_rt::test]
    async fn test_para() {
        let chunks = vec![
            para(Some(Properties {
                align: Some("end".to_owned()),
                ..Default::default()
            })),
            text(
                "Hello".to_owned(),
                Some(Properties {
                    bold: Some(true),
                    ..Default::default()
                }),
            ),
            text(
                "Rust".to_owned(),
                Some(Properties {
                    italic: Some(true),
                    underline: Some(true),
                    font_size: Some(Px::new(32)),
                    ..Default::default()
                }),
            ),
            text(
                "!!!".to_owned(),
                Some(Properties {
                    background: Some("#123".to_owned()),
                    ..Default::default()
                }),
            ),
            end(),
            para(Some(Properties {
                align: Some("center".to_owned()),
                ..Default::default()
            })),
            hyperlink("https://webix.com".to_owned()),
            text(
                "Visit webix".to_owned(),
                Some(Properties {
                    underline: Some(true),
                    color: Some("#0066ff".to_owned()),
                    background: Some("#ff00ff".to_owned()),
                    ..Default::default()
                }),
            ),
            end(),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/styles.docx".to_owned());
    }

    #[actix_rt::test]
    async fn test_numbering() {
        let chunks = vec![
            para(Some(Properties {
                align: Some("end".to_owned()),
                ..Default::default()
            })),
            text(
                "Hello".to_owned(),
                Some(Properties {
                    bold: Some(true),
                    ..Default::default()
                }),
            ),
            text(
                "Rust".to_owned(),
                Some(Properties {
                    italic: Some(true),
                    underline: Some(true),
                    font_size: Some(Px::new(32)),
                    ..Default::default()
                }),
            ),
            text(
                "!!!".to_owned(),
                Some(Properties {
                    background: Some("#123".to_owned()),
                    ..Default::default()
                }),
            ),
            end(),
            ul(None),
            /**/ li(None),
            /**//**/
            text(
                "Kanban".to_owned(),
                Some(Properties {
                    font_size: Some(Px::new(32)),
                    ..Default::default()
                }),
            ),
            /**/ end(),
            /**/ li(None),
            /**//**/ text("To Do List".to_owned(), None),
            /**/ end(),
            /**/ ol(None),
            /**//**/ li(None),
            /**//**//**/ text("Label".to_owned(), None),
            /**//**/ end(),
            /**//**/ li(None),
            /**//**//**/ text("Due date".to_owned(), None),
            /**//**/ end(),
            /**//**/ ul(None),
            /**//**//**/ li(None),
            /**//**//**//**/ text("Time zone".to_owned(), None),
            /**//**//**/ end(),
            /**//**//**/ li(None),
            /**//**//**//**/ text("Time".to_owned(), None),
            /**//**//**/ end(),
            /**//**/ end(),
            /**//**/ li(None),
            /**//**//**/ text("Checked".to_owned(), None),
            /**//**/ end(),
            /**/ end(),
            /**/ li(None),
            /**//**/ text("Gantt".to_owned(), None),
            /**/ end(),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/numbering.docx".to_owned());
    }

    #[actix_rt::test]
    async fn test_base64_image() {
        let chunks = vec![
            para(None),
            text("Image from Base64 String: ".to_owned(), Some(Properties{
                font_size: Some(Px::new(32)),
                bold: Some(true),
                ..Default::default()
            })),
            image(&"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".to_owned(), 30, 30),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/image_base64.docx".to_owned());
    }

    #[actix_rt::test]
    async fn test_spacing() {
        let chunks = vec![
            para(None),
            text("Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(Some(Properties {
                line_height: Some("1.0".to_owned()),
                ..Default::default()
            })),
            text("1.0 Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(None),
            text("Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(Some(Properties {
                line_height: Some("2.0".to_owned()),
                ..Default::default()
            })),
            text("2.0 Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(Some(Properties {
                line_height: Some("3.0".to_owned()),
                ..Default::default()
            })),
            text("3.0 Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(Some(Properties {
                line_height: Some("4.0".to_owned()),
                ..Default::default()
            })),
            text("4.0 Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
            para(Some(Properties {
                line_height: Some("1.5".to_owned()),
                ..Default::default()
            })),
            text("1.5 Pariatur excepteur aute magna veniam commodo consectetur sit cupidatat non dolor minim adipisicing voluptate in.".to_owned(), None),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/spacing.docx".to_owned());
    }

    #[actix_rt::test]
    async fn test_sub_super_script() {
        let chunks = vec![
            para(None),
            /**/ text("text".to_owned(), None),
            /**/ spr(),
            /**//**/ text("superscripted".to_owned(), None),
            /**/ end(),
            end(),
            para(None),
            /**/ text("text".to_owned(), None),
            /**/ sub(),
            /**//**/ text("subscripted".to_owned(), None),
            /**/ end(),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/sub-super-script.docx".to_owned());
    }

    #[actix_rt::test]
    async fn test_break_and_newline() {
        let chunks = vec![
            para(None),
            /**/text(
                "Text examle: ".to_owned(),
                Some(Properties {
                    bold: Some(true),
                    font_size: Some(Px::new(22)),
                    ..Default::default()
                }),
            ),
            /**/text("Lorem ipsum dolor sit amet, consectetur adipiscing elit. Nulla maximus:".to_owned(), None),
            /**/text("lorem vitae tellus bibendum, ut vulputate ante consectetur. Vestibulum tristique faucibus dolor, et imperdiet massa pulvinar ullamcorper".to_owned(), None),
            end(),
            br(),
            br(),
            br(),
            para(None),
            /**/text(
                "Text examle with Newline: ".to_owned(),
                Some(Properties {
                    bold: Some(true),
                    font_size: Some(Px::new(22)),
                    ..Default::default()
                }),
            ),
            /**/ text("Lorem ipsum dolor sit amet, consectetur adipiscing elit. Nulla maximus:".to_owned(), None),
            /**/ nl(),
            /**/ text("lorem vitae tellus bibendum, ut vulputate ante consectetur. Vestibulum tristique faucibus dolor, et imperdiet massa pulvinar ullamcorper".to_owned(), None),
            end(),
        ];

        let mut d = DocxDocument::new();

        let bytes = d.from_chunks(chunks).await;
        save_docx(bytes, "./temp/output/break-newline.docx".to_owned());
    }
}
