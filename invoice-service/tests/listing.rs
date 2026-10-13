//! Invoice listing: state filter and cursor pagination.
//! Added after `GET /invoices?limit=1` was found returning 400 while writing the API docs.

mod common;

use std::collections::HashSet;

#[tokio::test]
async fn list_invoices_filters_by_state_and_paginates() {
    let app = common::spawn_app().await;
    let mut open_ids = HashSet::new();
    for _ in 0..3 {
        open_ids.insert(app.open_invoice(1_000).await);
    }
    let paid = app.open_invoice(1_000).await;
    assert_eq!(app.pay(&paid, "tok_success", "list-1").await.0, 200);

    // Walk the open invoices one page at a time.
    let mut seen = HashSet::new();
    let mut cursor = String::new();
    loop {
        let page = app.get(&format!("/invoices?state=open&limit=1{cursor}")).await;
        let data = page["data"].as_array().unwrap_or_else(|| panic!("{page}"));
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["state"], "open");
        let id = data[0]["id"].as_str().unwrap().to_string();
        assert!(seen.insert(id.clone()), "page repeated {id}");
        if page["has_more"] == false {
            break;
        }
        cursor = format!("&starting_after={id}");
    }
    assert_eq!(seen, open_ids);

    let paid_page = app.get("/invoices?state=paid").await;
    assert_eq!(paid_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(paid_page["data"][0]["id"], paid.as_str());
}
