// Convert MHTML files from posts in a Google Group to a site.
//
// This code focuses on the case where the posts are focused on displaying photos.

pub mod mhtml;
pub mod thumbnail;
pub mod utf8_bytes;

use chrono::{DateTime, FixedOffset, NaiveDate};
use clap::Parser;
// Using feature "unescape"
use htmlize;

use flume;
use lol_html::{element, rewrite_str, RewriteStrSettings};
use regex;
use regex::bytes::Regex;
use scraper::{Html, Selector};
use serde_derive::Serialize;
use serde_json;
use threadpool;
// Adds unicode_truncate method to str.
use unicode_truncate::UnicodeTruncateStr;

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::sync::OnceLock;
use std::vec::Vec;

const INITIAL_TEXT_MAX_LEN: usize = 140;
const MIN_I_TEXT_LEN: usize = 3;
const MAX_I_TEXT_LEN: usize = 50;

/// Generate a site from a directory of Google Group MHTML files.
#[derive(Parser)]
#[command(rename_all = "snake_case")]
struct Cli {
    /// Path to the directory of .mhtml files.
    #[arg(short, long, value_name = "DIR")]
    input_dir: std::path::PathBuf,

    /// Path to the directory for output files.
    #[arg(short, long, value_name = "DIR")]
    output_dir: std::path::PathBuf,
}

#[derive(Default)]
struct GroupsPost {
    author: Option<String>,
    /// Date extracted from the post.
    date: Option<NaiveDate>,
    /// HTML fragment for the main post.
    html: String,
    /// URLs of images used within post_html.
    image_urls: Vec<String>,
    /// Text from i tags, in order of first unique appearance.
    i_text: Vec<String>,
}

#[derive(Default, Serialize)]
struct Page {
    title: String,
    /// Date on which the content was scraped.
    scrape_date: DateTime<FixedOffset>,
    /// Best guess as to when it was originally posted.
    post_date: NaiveDate,
    /// Original URL at which the post appeared.
    original_url: String,
    /// Name within output dir.
    output_file: String,
    /// Name within output dir.
    images_dir: String,
    /// A segment of text from the beginning of the post, stripped of HTML.
    initial_text: String,
    /// Paths to thumbnails for images within images_dir.
    thumbnails: Vec<String>,
    /// Text from i tags, in order of first unique appearance.
    i_text: Vec<String>,
}

fn calculate_hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn invalid_data_err(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn date_from_title(title: &[u8]) -> Option<NaiveDate> {
    static DATE_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let date_re = DATE_RE_LOCK
        .get_or_init(|| Regex::new(r#"(?P<month>\d+)/(?P<day>\d+)/(?P<year>\d+)"#).unwrap());
    let captures = date_re.captures(title)?;
    let year: i32 = utf8_bytes::to_str(&captures["year"]).parse().unwrap();
    let month: u32 = utf8_bytes::to_str(&captures["month"]).parse().unwrap();
    let day: u32 = utf8_bytes::to_str(&captures["day"]).parse().unwrap();
    let full_year = if year < 100 { year + 2000 } else { year };
    return NaiveDate::from_ymd_opt(full_year, month, day);
}

fn get_text_from_html(html: &String) -> String {
    static HTML_RE_LOCK: OnceLock<regex::Regex> = OnceLock::new();
    static MULTI_SPACE_RE_LOCK: OnceLock<regex::Regex> = OnceLock::new();
    static SPACE_PUNCTUATION_RE_LOCK: OnceLock<regex::Regex> = OnceLock::new();
    let html_re = HTML_RE_LOCK.get_or_init(|| regex::Regex::new(r"<[^>]+>").unwrap());
    let multi_space_re = MULTI_SPACE_RE_LOCK.get_or_init(|| regex::Regex::new(r"\s+").unwrap());
    let space_punctuation_re =
        SPACE_PUNCTUATION_RE_LOCK.get_or_init(|| regex::Regex::new(r#" ([,.!:;?])"#).unwrap());
    // Replace tags with spaces to avoid running visually separated text together.
    let stripped = html_re.replace_all(html, " ");
    // Compress runs of spaces.
    let compressed = multi_space_re.replace_all(&stripped, " ");
    // Strip spaces before certain punctuation marks.
    let despaced = space_punctuation_re.replace_all(&compressed, "$1");
    return htmlize::unescape(despaced).trim().to_string();
}

fn date_from_html(html: &[u8]) -> Option<NaiveDate> {
    static DATETIME_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let datetime_re = DATETIME_RE_LOCK.get_or_init(|| {
        Regex::new(r#"<span[^>]*>\s*(?P<date>[A-Z][a-z]{2} \d+, \d{4}, \d{1,2}:\d\d:\d\d[^<]+(?:AM|PM))</span>"#)
            .unwrap()
    });
    let captures = datetime_re.captures(html)?;
    // u202F = NARROW NO-BREAK SPACE
    let datetime_str = utf8_bytes::to_string(&captures["date"]);
    let (date, _remainder) = NaiveDate::parse_and_remainder(&datetime_str, "%b %d, %Y").unwrap();
    return Some(date);
}

fn rewrite_i_tags(html: &String, i_texts: &mut Vec<String>) -> String {
    static REPEATED_I_RE_LOCK: OnceLock<regex::Regex> = OnceLock::new();
    static I_RE_LOCK: OnceLock<regex::Regex> = OnceLock::new();
    // Find sequences of one or more i tags. We'll merge them.
    let repeated_i_re = REPEATED_I_RE_LOCK
        .get_or_init(|| regex::Regex::new(r#"(<i>.*?</i>(\s|&nbsp;)*)+"#).unwrap());
    // Identify start/end i tags.
    let i_re = I_RE_LOCK.get_or_init(|| regex::Regex::new(r#"</?i>"#).unwrap());
    let mut known_i_texts: HashSet<String> = HashSet::new();
    return repeated_i_re
        .replace_all(html, |caps: &regex::Captures| {
            let i_html = caps.get(0).unwrap().as_str();
            let output_html = format!("<i>{}</i>", i_re.replace_all(i_html, ""));
            let text = get_text_from_html(&String::from(output_html.clone()));
            // Without the explict return, rustfmt is stripping away the curly braces, which
            // in turn causes the `||` operator to get interpreted as introducing another
            // closure.
            let text = String::from(text.trim_matches(|c: char| {
                return c.is_ascii_punctuation() || c.is_ascii_whitespace();
            }));
            let text_len = text.len();
            if text_len >= MIN_I_TEXT_LEN && text_len <= MAX_I_TEXT_LEN {
                // If we get a true value when attempting to add it to the set, that means
                // it was unknown previously.
                if known_i_texts.insert(text.clone()) {
                    i_texts.push(text.clone());
                }
            }
            return output_html;
        })
        .to_string();
}

fn parse_groups_post(html: &[u8]) -> Result<GroupsPost, io::Error> {
    let mut post: GroupsPost = Default::default();
    post.date = date_from_html(html);
    let fragment = Html::parse_fragment(&utf8_bytes::to_str(html));
    let listitem_selector = Selector::parse(r#"section[role="listitem"]"#).unwrap();
    let Some(section) = fragment.select(&listitem_selector).next() else {
        return Err(invalid_data_err("Post has no section[role=listitem]"));
    };
    if let Some(author) = section.value().attr("data-author") {
        post.author = Some(String::from(author));
    };
    let region_selector = Selector::parse(r#"[role="region"]"#).unwrap();
    let Some(region) = section.select(&region_selector).next() else {
        return Err(invalid_data_err("Post has no [role=region]"));
    };
    let img_selector = Selector::parse("img").unwrap();
    for img in region.select(&img_selector) {
        if let Some(src) = img.attr("src") {
            post.image_urls
                .push(String::from(src).replace("&amp;", "&"));
        }
    }
    post.html = rewrite_i_tags(&region.inner_html(), &mut post.i_text);
    Ok(post)
}

fn parse_post_from_mhtml_piece(piece: &mhtml::MhtmlPiece) -> Result<GroupsPost, io::Error> {
    if piece.content_type != "text/html" {
        return Err(invalid_data_err("Expecting text/html"));
    }
    return parse_groups_post(&piece.bytes);
}

fn make_output_html_for_post(
    post: &GroupsPost,
    page: &Page,
    image_to_path: &HashMap<String, String>,
) -> String {
    let mut img_count = 0;
    let element_content_handlers = vec![
        // Rewrite image links to point to local copies if available.
        element!("img[src]", |el| {
            let src = el.get_attribute("src").unwrap().replace("&amp;", "&");
            if let Some(path) = image_to_path.get(&src) {
                img_count += 1;
                el.set_attribute("src", &path).unwrap();
                el.set_attribute("id", &format!("img-{img_count}")).unwrap();
            }

            Ok(())
        }),
        // Strip attributes other than href and src.
        element!("*", |el| {
            let attribute_names: Vec<String> = el.attributes().iter().map(|x| x.name()).collect();
            for attribute in attribute_names {
                if attribute != "href"
                    && attribute != "src"
                    && !(el.tag_name() == "img" && attribute == "id")
                {
                    el.remove_attribute(&attribute.as_str());
                }
            }

            Ok(())
        }),
    ];
    let output_post_html = rewrite_str(
        &post.html.as_str(),
        RewriteStrSettings {
            element_content_handlers,
            ..RewriteStrSettings::new()
        },
    )
    .unwrap();
    let mut info_pieces: Vec<String> = Vec::new();
    if let Some(author) = &post.author {
        info_pieces.push(author.clone());
    }
    info_pieces.push(page.post_date.format("%b %d, %Y").to_string());

    format!(
        r#"<!DOCTYPE html>
<html lang='en'>
    <head>
        <title>{title}</title>
    <meta charset='utf-8'>
    </head>
    <body>
        <h1>{title}</h1>
        <p>{info}</p>
        {post_html}
        <p>
          <i>Scraped on {scrape_date} from <a href="{original_url}">{original_url}</a></i>
        </p>
    </body>
</html>"#,
        post_html = output_post_html,
        title = page.title,
        info = info_pieces.join(", "),
        scrape_date = page.scrape_date,
        original_url = page.original_url
    )
}

fn get_initial_text_from_html(html: &String) -> String {
    let text = get_text_from_html(html);
    let (truncated, _) = text.unicode_truncate(INITIAL_TEXT_MAX_LEN);
    let mut result = truncated.to_string();
    if result.len() < text.len() {
        result.push_str("...");
    }
    result
}

fn create_page_from_mhtml(
    path: &std::path::PathBuf,
    output_dir: &std::path::PathBuf,
) -> Result<Page, io::Error> {
    let mut page: Page = Default::default();

    let doc = mhtml::parse(&mut fs::read(path)?)?;
    page.title = doc.subject;
    page.scrape_date = doc.date;
    page.original_url = doc.location;

    let mut flattened_title = page.title.replace("/", "_").replace(" ", "_");
    flattened_title.retain(|c| c.is_ascii_alphanumeric() || c == '_');
    flattened_title.make_ascii_lowercase();
    let basename = format!(
        "{}_{:x}",
        flattened_title,
        calculate_hash(&page.original_url)
    );
    page.output_file = format!("{}.html", basename);
    page.images_dir = format!("{}_images", basename);

    let mut image_to_path: HashMap<String, String> = HashMap::new();
    let mut image_to_thumbnail: HashMap<String, String> = HashMap::new();
    let mut num_images = 0;
    let images_dir = output_dir.join(&page.images_dir);
    fs::create_dir_all(&images_dir)?;

    if doc.pieces.is_empty() {
        return Err(invalid_data_err("MHTML has no data"));
    };
    let post = parse_post_from_mhtml_piece(&doc.pieces[0])?;

    for piece in doc.pieces.iter().skip(1) {
        if piece.content_type == "image/jpeg" && post.image_urls.contains(&piece.location) {
            num_images += 1;
            let filename = format!("{:03}.jpeg", num_images);
            image_to_path.insert(
                piece.location.clone(),
                format!("{}/{}", &page.images_dir, &filename),
            );
            fs::write(images_dir.join(&filename), &piece.bytes)?;
            let thumbnail_filename = format!("{:03}_thumbnail.jpeg", num_images);
            thumbnail::create_thumbnail(&piece.bytes, &images_dir.join(&thumbnail_filename));
            image_to_thumbnail.insert(
                piece.location.clone(),
                format!("{}/{}", page.images_dir, thumbnail_filename),
            );
        }
    }
    for image_url in &post.image_urls {
        if let Some(thumbnail_path) = image_to_thumbnail.get(image_url) {
            page.thumbnails.push(thumbnail_path.clone());
        }
    }
    if let Some(post_date) = post.date {
        page.post_date = post_date;
    } else if let Some(title_date) = date_from_title(&page.title.as_bytes()) {
        page.post_date = title_date;
    } else {
        page.post_date = page.scrape_date.naive_local().date();
    }

    let output_html = make_output_html_for_post(&post, &page, &image_to_path);
    fs::write(output_dir.join(&page.output_file), &output_html.as_bytes())?;
    page.initial_text = get_initial_text_from_html(&post.html);
    page.i_text = post.i_text;

    Ok(page)
}

struct Site {
    /// Number of pages generated from posts.
    num_pages: usize,
}

fn create_site_from_mhtml_dir(
    input_dir: &std::path::PathBuf,
    output_dir: &std::path::PathBuf,
) -> Result<Site, io::Error> {
    let mut num_pages = 0;
    // TODO: make the number of workers configurable.
    let pool = threadpool::ThreadPool::new(5);
    let (sender, receiver) = flume::unbounded();
    for entry in fs::read_dir(input_dir)? {
        let entry = entry?;
        if entry.file_name().to_str().unwrap().ends_with(".mhtml") {
            num_pages += 1;
            let path = entry.path();
            let my_output_dir = output_dir.clone();
            let sender = sender.clone();
            pool.execute(move || {
                sender
                    .send(create_page_from_mhtml(&path, &my_output_dir))
                    .unwrap();
            });
        }
    }
    let mut pages: Vec<Page> = vec![];
    for result in receiver.iter().take(num_pages) {
        pages.push(result?);
    }
    pool.join();

    pages.sort_by(|a, b| {
        if a.post_date == b.post_date {
            a.title.partial_cmp(&b.title).unwrap()
        } else {
            // Put more recent posts first
            b.post_date.partial_cmp(&a.post_date).unwrap()
        }
    });
    fs::write(
        output_dir.join("posts.json"),
        serde_json::to_string(&pages)?,
    )?;

    Ok(Site {
        num_pages: num_pages,
    })
}

fn main() {
    let args = Cli::parse();
    fs::create_dir_all(&args.output_dir).unwrap();
    let site = create_site_from_mhtml_dir(&args.input_dir, &args.output_dir).unwrap();
    println!(
        "Generated {:?} pages under {:?}",
        site.num_pages,
        args.output_dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_from_title_yy() {
        assert_eq!(
            date_from_title(b"7/19/23"),
            NaiveDate::from_ymd_opt(2023, 7, 19)
        );
    }

    #[test]
    fn date_from_title_yyyy() {
        assert_eq!(
            date_from_title(b"7/19/2023"),
            NaiveDate::from_ymd_opt(2023, 7, 19)
        );
    }

    #[test]
    fn date_from_html_missing() {
        assert_eq!(date_from_html(b""), None);
    }

    #[test]
    fn date_from_html_pm() {
        assert_eq!(
            date_from_html(br#"<span class="zX2W9c">Jul 13, 2023, 7:31:18\u{202F}PM</span>"#),
            NaiveDate::from_ymd_opt(2023, 7, 13)
        );
    }

    #[test]
    fn get_initial_text_from_html_empty() {
        assert_eq!(get_initial_text_from_html(&String::from("")), "");
    }

    #[test]
    fn get_initial_text_from_html_all_spaces() {
        assert_eq!(
            get_initial_text_from_html(&String::from(" <p> <span>   </span></p>   ")),
            ""
        );
    }

    #[test]
    fn get_initial_text_from_html_compress_spaces() {
        assert_eq!(
            get_initial_text_from_html(&String::from(" <p>Hi,</p><p>there<b>!</b>")),
            "Hi, there!"
        );
    }

    #[test]
    fn rewrite_i_tags_space_separated() {
        let mut i_texts: Vec<String> = vec![];
        let html = rewrite_i_tags(
            &String::from("<i>hi</i> <i>there</i><br><i>hi there</i>"),
            &mut i_texts,
        );
        assert_eq!(i_texts, vec!["hi there"]);
        assert_eq!(html, "<i>hi there</i><br><i>hi there</i>");
    }

    #[test]
    fn rewrite_i_tags_with_nbsp() {
        let mut i_texts: Vec<String> = vec![];
        let html = rewrite_i_tags(&String::from("<i>hi</i>&nbsp;<i>there</i>"), &mut i_texts);
        assert_eq!(i_texts, vec!["hi\u{a0}there"]);
        assert_eq!(html, "<i>hi&nbsp;there</i>");
    }
}
