// SPDX-License-Identifier: AGPL-3.0-only

// On Windows the .exe carries the app's icon as a resource; the window's own icon is set at start.
fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/beavermail.ico");
        resource.compile().expect("the Windows resources compile");
    }
}
