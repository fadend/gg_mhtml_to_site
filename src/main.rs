// Convert MHTML files from posts in a Google Group to a site.
//
// This code focuses on the case where the posts are focused on displaying photos.

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use chrono::{DateTime, FixedOffset, NaiveDate};
use clap::Parser;
use lol_html::{element, rewrite_str, RewriteStrSettings};
use quoted_printable;
use regex::bytes::Regex;
use scraper::{Html, Selector};

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::sync::OnceLock;
use std::vec::Vec;

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
    /// May be relative to current time, e.g., "5:39 AM (8 hours ago)"
    datetime_str: Option<String>,
    /// HTML fragment for the main post.
    html: String,
    /// URLs of images used within post_html.
    image_urls: Vec<String>,
}

#[derive(Default)]
struct Page {
    title: String,
    /// Date on which the content was scraped
    scrape_date: DateTime<FixedOffset>,
    /// Best guess as to when it was originally posted.
    post_date: NaiveDate,
    /// Original URL at which the post appeared.
    original_url: String,
    /// Name within output dir.
    output_file: String,
    /// Name within output dir.
    images_dir: String,
}

#[derive(Default)]
struct MhtmlPiece {
    content_type: String,
    location: String,
    bytes: Vec<u8>,
}

fn calculate_hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn utf8_bytes_to_str(bytes: &[u8]) -> &str {
    &std::str::from_utf8(bytes).unwrap()
}

fn utf8_bytes_to_string(bytes: &[u8]) -> String {
    String::from(utf8_bytes_to_str(bytes))
}

fn invalid_data_err(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn decode_base64_containing_whitespace(data: &[u8]) -> Vec<u8> {
    let mut copy = Vec::from(data);
    copy.retain(|b| !b.is_ascii_whitespace());
    BASE64_STANDARD.decode(copy).unwrap()
}

fn datetime_str_from_html(html: &[u8]) -> Option<String> {
    static DATETIME_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let datetime_re = DATETIME_RE_LOCK.get_or_init(|| {
        Regex::new(r#"<span[^>]*>\s*(?P<date>[^<>]+\(\d+ (?:hour|day)s? ago\))[^<>]+</span>"#)
            .unwrap()
    });
    let captures = datetime_re.captures(html)?;
    // u202F = NARROW NO-BREAK SPACE
    let datetime_str = utf8_bytes_to_string(&captures["date"])
        .replace("\u{202F}", " ")
        .replace("&nbsp;", " ");
    return Some(String::from(datetime_str.trim()));
}

fn parse_groups_post(html: &[u8]) -> Result<GroupsPost, io::Error> {
    let mut post: GroupsPost = Default::default();
    post.datetime_str = datetime_str_from_html(html);
    let fragment = Html::parse_fragment(&utf8_bytes_to_str(html));
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
    post.html = region.inner_html();
    Ok(post)
}

fn parse_mhtml_piece(text: &[u8]) -> Result<MhtmlPiece, io::Error> {
    static SECTION_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let section_re = SECTION_RE_LOCK.get_or_init(|| {
        Regex::new(
            r#"(?x)^Content-Type:\s(?P<content_type>\S+)\s*
(?:Content-ID:\s\S+\s+)?
Content-Transfer-Encoding:\s(?P<encoding>\S+)\s*
Content-Location:\s(?P<location>\S+)\s*"#,
        )
        .unwrap()
    });
    let Some(captures) = section_re.captures(text) else {
        println!("Problem parsing: <<{}>>", utf8_bytes_to_str(text));
        return Err(invalid_data_err("MHTML piece doesn't have expected header"));
    };
    let mut piece: MhtmlPiece = Default::default();
    piece.content_type = utf8_bytes_to_string(&captures["content_type"]);
    piece.location = utf8_bytes_to_string(&captures["location"]);
    let remainder = &text[captures.get(0).unwrap().end()..];
    let encoding = utf8_bytes_to_str(&captures["encoding"]);
    piece.bytes = match encoding {
        "base64" => decode_base64_containing_whitespace(remainder),
        "quoted-printable" => {
            quoted_printable::decode(remainder, quoted_printable::ParseMode::Strict).unwrap()
        }
        _ => panic!("Unknown encoding {} for {}", &encoding, &piece.location),
    };

    Ok(piece)
}

fn parse_post_from_mhtml_piece(text: &[u8]) -> Result<GroupsPost, io::Error> {
    let piece = parse_mhtml_piece(text)?;
    if piece.content_type != "text/html" {
        return Err(invalid_data_err("Expecting text/html"));
    }
    return parse_groups_post(&piece.bytes);
}

fn parse_origin_date(scrape_date: &DateTime<FixedOffset>, post_date: &Option<String>) -> NaiveDate {
    if let Some(post_date_str) = &post_date {
        if post_date_str.contains("day") {
            if let Some(left_paren) = post_date_str.find(" (") {
                let prefix: String = post_date_str.chars().take(left_paren).collect();
                let (date, _remainder) =
                    NaiveDate::parse_and_remainder(&prefix.as_str(), "%b %d, %Y").unwrap();
                return date;
            }
        }
    }
    return scrape_date.naive_local().date();
}

fn make_output_html_for_post(
    post: &GroupsPost,
    page: &Page,
    image_to_path: &HashMap<String, String>,
) -> String {
    let element_content_handlers = vec![
        // Rewrite image links to point to local copies if available.
        element!("img[src]", |el| {
            let src = el.get_attribute("src").unwrap().replace("&amp;", "&");
            if let Some(path) = image_to_path.get(&src) {
                el.set_attribute("src", &path).unwrap();
            }

            Ok(())
        }),
        // Strip attributes other than href and src.
        element!("*", |el| {
            let attribute_names: Vec<String> = el.attributes().iter().map(|x| x.name()).collect();
            for attribute in attribute_names {
                if attribute != "href" && attribute != "src" {
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

fn create_page_from_mhtml(
    path: &std::path::PathBuf,
    output_dir: &std::path::PathBuf,
) -> Result<Page, io::Error> {
    static HEADER_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let header_re = HEADER_RE_LOCK.get_or_init(|| {
        Regex::new(
            r#"(?x)^From:\s[^\r\n]+\s*
Snapshot-Content-Location:\s(?P<location>[^\r\n]+)\s*
Subject:\s(?P<subject>[^\r\n]+)\s*
Date:\s(?<scrape_date>[^\r\n]+)\s*
MIME-Version:\s[^\r\n]+\s*
Content-Type:\s[^\r\n]+\s*
\s+type=[^\r\n]+
\s+boundary="(?P<boundary>[^"]+)""#,
        )
        .unwrap()
    });

    let mut page: Page = Default::default();
    let contents = fs::read(path)?;
    let mut contents_slice: &[u8] = &contents;
    let Some(header_captures) = header_re.captures(contents_slice) else {
        return Err(invalid_data_err("MHTML doesn't have expected header"));
    };
    page.title = utf8_bytes_to_string(&header_captures["subject"]);
    page.scrape_date =
        DateTime::parse_from_rfc2822(&utf8_bytes_to_str(&header_captures["scrape_date"])).unwrap();
    page.original_url = utf8_bytes_to_string(&header_captures["location"]);

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

    // Skip past the header, matched by the Regex.
    let full_match = header_captures.get(0).unwrap();
    contents_slice = &contents_slice[full_match.end()..];

    let mut image_to_path: HashMap<String, String> = HashMap::new();
    let mut num_images = 0;
    let images_dir = output_dir.join(&page.images_dir);
    fs::create_dir_all(&images_dir)?;

    let mut boundary_pattern: Vec<u8> = Vec::new();
    header_captures.expand(br#"[\r\n]*-*$boundary-*[\r\n]*"#, &mut boundary_pattern);
    let boundary_re = Regex::new(&utf8_bytes_to_str(&boundary_pattern)).unwrap();

    let mut pieces_iter = boundary_re.split(contents_slice).filter(|x| !x.is_empty());

    let Some(first_raw_piece) = pieces_iter.next() else {
        return Err(invalid_data_err("MHTML has no data"));
    };
    let post = parse_post_from_mhtml_piece(first_raw_piece)?;
    for raw_piece in pieces_iter {
        let piece = parse_mhtml_piece(raw_piece)?;
        if piece.content_type == "image/jpeg" && post.image_urls.contains(&piece.location) {
            num_images += 1;
            let filename = format!("{:03}.jpeg", num_images);
            image_to_path.insert(
                piece.location,
                format!("{}/{}", &page.images_dir, &filename),
            );
            fs::write(images_dir.join(&filename), &piece.bytes)?;
        }
    }
    page.post_date = parse_origin_date(&page.scrape_date, &post.datetime_str);

    let output_html = make_output_html_for_post(&post, &page, &image_to_path);
    fs::write(output_dir.join(&page.output_file), &output_html.as_bytes())?;

    Ok(page)
}

struct Site {
    /// Number of pages generated from posts.
    num_pages: i32,
}

fn make_pages_index_html(pages: &Vec<Page>) -> String {
    let mut items: Vec<String> = Vec::new();
    for page in pages {
        items.push(format!(
            r#"<li> <a href="{}">{}</a> ({})"#,
            page.output_file,
            page.title,
            page.post_date.format("%b %d, %Y").to_string()
        ));
    }

    format!(
        r#"<!DOCTYPE html>
    <html lang='en'>
        <head>
            <title>Posts Index</title>
        <meta charset='utf-8'>
        </head>
        <body>
            <h1>Posts Index</h1>
            <ul>
                {}
            </ul>
        </body>
    </html>"#,
        items.join("")
    )
}

fn create_site_from_mhtml_dir(
    input_dir: &std::path::PathBuf,
    output_dir: &std::path::PathBuf,
) -> Result<Site, io::Error> {
    let mut site = Site { num_pages: 0 };
    let mut pages: Vec<Page> = Vec::new();
    for entry in fs::read_dir(input_dir)? {
        let entry = entry?;
        if entry.file_name().to_str().unwrap().ends_with(".mhtml") {
            pages.push(create_page_from_mhtml(&entry.path(), output_dir)?);
            site.num_pages += 1;
        }
    }
    pages.sort_by(|a, b| {
        if a.post_date == b.post_date {
            a.title.partial_cmp(&b.title).unwrap()
        } else {
            a.post_date.partial_cmp(&b.post_date).unwrap()
        }
    });
    let index_html = make_pages_index_html(&pages);
    fs::write(output_dir.join("index.html"), index_html.as_bytes())?;

    Ok(site)
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
    fn parse_origin_date_hours_ago() {
        let scrape_date = DateTime::parse_from_rfc2822("Mon, 7 Oct 2024 09:08:15 -0700").unwrap();
        let result = parse_origin_date(&scrape_date, &Some(String::from("5:29 AM (5 hours ago)")));
        assert_eq!(result, NaiveDate::from_ymd_opt(2024, 10, 7).unwrap());
    }

    #[test]
    fn parse_origin_date_missing_post_date() {
        let scrape_date = DateTime::parse_from_rfc2822("Mon, 7 Oct 2024 10:09:30 -0700").unwrap();
        let result = parse_origin_date(&scrape_date, &None);
        assert_eq!(result, NaiveDate::from_ymd_opt(2024, 10, 7).unwrap());
    }

    #[test]
    fn parse_origin_date_days_ago() {
        let scrape_date = DateTime::parse_from_rfc2822("Mon, 7 Oct 2024 10:09:30 -0700").unwrap();
        let result = parse_origin_date(
            &scrape_date,
            &Some(String::from("Oct 3, 2024, 9:55:19 AM (7 days ago)")),
        );
        assert_eq!(result, NaiveDate::from_ymd_opt(2024, 10, 3).unwrap());
    }

    #[test]
    fn datetime_str_from_html_1_hour_ago() {
        let input = r#"<span class="zX2W9c">5:29 AM&nbsp;(1 hour ago)&nbsp;</span>"#.as_bytes();
        let result = datetime_str_from_html(input);
        assert_eq!(result, Some(String::from("5:29 AM (1 hour ago)")));
    }

    #[test]
    fn datetime_str_from_html_2_days_ago() {
        let input =
            r#"<span class="zX2W9c">Oct 8, 2024, 5:10:58 AM&nbsp;(2 days ago)&nbsp;</span>"#
                .as_bytes();
        let result = datetime_str_from_html(input);
        assert_eq!(
            result,
            Some(String::from("Oct 8, 2024, 5:10:58 AM (2 days ago)"))
        );
    }
}
