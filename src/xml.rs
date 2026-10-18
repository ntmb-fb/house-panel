//! Just enough XML for the flat documents BluOS and Onkyo send.

/// Text of the first `<name>…</name>`, unescaped.
pub fn tag(xml: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let len = xml[start..].find(&format!("</{name}>"))?;
    Some(unescape(&xml[start..start + len]))
}

/// Value of `name="…"` inside one element's text, unescaped.
pub fn attr(element: &str, name: &str) -> Option<String> {
    let start = element.find(&format!(" {name}=\""))? + name.len() + 3;
    let len = element[start..].find('"')?;
    Some(unescape(&element[start..start + len]))
}

/// The text of every `<name …>` start tag.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name} ");
    xml.match_indices(&open)
        .filter_map(|(i, _)| xml[i..].find('>').map(|end| &xml[i..i + end]))
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bluos_and_onkyo_samples() {
        let status = r#"<status etag="f2e4"><mute>0</mute><title1>Rock &amp; Roll</title1><volume>24</volume></status>"#;
        assert_eq!(tag(status, "volume").as_deref(), Some("24"));
        assert_eq!(tag(status, "title1").as_deref(), Some("Rock & Roll"));
        assert_eq!(tag(status, "artist"), None);
        assert_eq!(attr(status, "etag").as_deref(), Some("f2e4"));

        let nri = r#"<selectorlist count="2"><selector id="10" value="1" name="BD/DVD" zone="07" /><selector id="2b" value="1" name="NET" zone="07" /></selectorlist>"#;
        let sel = elements(nri, "selector");
        assert_eq!(sel.len(), 2);
        assert_eq!(attr(sel[1], "id").as_deref(), Some("2b"));
        assert_eq!(attr(sel[0], "name").as_deref(), Some("BD/DVD"));
        assert!(elements(nri, "selectorlist").len() == 1);
    }
}
