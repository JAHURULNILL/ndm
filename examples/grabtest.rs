fn main() {
    let url = std::env::args().nth(1).expect("url");
    match ndm::grab_page(&url) {
        Ok(page) => {
            println!("title: {:?}", page.title);
            for it in &page.items {
                println!("- {} | label={:?} | size={:?}", it.url, it.label, it.size);
            }
            println!("items: {}", page.items.len());
        }
        Err(e) => println!("ERROR: {e}"),
    }
}
