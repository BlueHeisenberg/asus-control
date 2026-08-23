fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("app.manifest");
    if let Err(e) = res.compile() {
        panic!("failed to compile resource: {e}");
    }
}
