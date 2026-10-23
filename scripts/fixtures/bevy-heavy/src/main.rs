use bevy::prelude::*;
const MARKER: &str = "version-A";
fn main() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.world_mut().spawn((Mesh3d::default(), MeshMaterial3d::<StandardMaterial>::default(), Transform::default()));
    app.update();
    println!("{MARKER}:entities={}", app.world().entities().len());
    if let Some(ms) = std::env::var("PROBE_SLEEP_MS").ok().and_then(|s| s.parse::<u64>().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
