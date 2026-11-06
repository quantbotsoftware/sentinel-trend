use super::*;

const SAMPLE_ATOM: &str = r#"<?xml version="1.0" encoding="ISO-8859-1" ?>
<feed xmlns="http://www.w3.org/2005/Atom">
<title>Latest Filings</title>
<entry>
<title>8-K - ACME THERAPEUTICS INC (0001234567) (Filer)</title>
<link rel="alternate" type="text/html" href="https://www.sec.gov/Archives/edgar/data/1234567/000123456726000001-index.htm"/>
<id>urn:tag:sec.gov,2008:accession-number=0001234567-26-000001</id>
<updated>2026-08-11T07:35:02-04:00</updated>
</entry>
<entry>
<title>424B5 - GAPPER PHARMA CORP (0007654321) (Filer)</title>
<link rel="alternate" type="text/html" href="https://www.sec.gov/Archives/edgar/data/7654321/000765432126000002-index.htm"/>
<id>urn:tag:sec.gov,2008:accession-number=0007654321-26-000002</id>
<updated>2026-08-11T07:36:10-04:00</updated>
</entry>
<entry>
<title>malformed title without form separator</title>
<id>urn:broken</id>
<updated>2026-08-11T07:37:00-04:00</updated>
</entry>
</feed>"#;

#[test]
fn parses_atom_entries_with_cik_and_link() {
    let filings = parse_atom(SAMPLE_ATOM);
    assert_eq!(filings.len(), 2, "malformed entry skipped");

    assert_eq!(filings[0].form, "8-K");
    assert_eq!(filings[0].cik, 1234567);
    assert!(filings[0].url.contains("000123456726000001-index.htm"));
    assert!(filings[0].entry_id.contains("0001234567-26-000001"));
    assert_eq!(filings[0].updated, "2026-08-11T07:35:02-04:00");

    assert_eq!(filings[1].form, "424B5");
    assert_eq!(filings[1].cik, 7654321);
}

#[test]
fn offering_form_classification() {
    for f in ["424B5", "424B4", "424B3", "S-3", "S-3/A", "S-1", "F-1", "F-3ASR"] {
        assert!(is_offering_form(f), "{f} should flag offering risk");
    }
    for f in ["8-K", "10-Q", "10-K", "SC 13G", "4"] {
        assert!(!is_offering_form(f), "{f} must not flag offering risk");
    }
}

#[test]
fn offering_cache_records_and_expires_by_symbol() {
    let now = chrono::Utc::now().timestamp();
    record_offering("gpph", now); // lowercase in — uppercased at boundary (S03)
    assert!(has_recent_offering("GPPH"));
    assert!(has_recent_offering("gpph"));
    assert!(!has_recent_offering("OTHER"));

    // Stale entry (25h old) must not flag.
    record_offering("STALE", now - 25 * 3600);
    assert!(!has_recent_offering("STALE"));

    daily_reset();
    assert!(!has_recent_offering("GPPH"));
}

#[test]
fn empty_or_garbage_atom_yields_no_filings() {
    assert!(parse_atom("").is_empty());
    assert!(parse_atom("<html>rate limited</html>").is_empty());
}
