//! Feature gates: build with `--no-default-features` to drop optional extensions.
use mlp_core::media::sender::MediaSenderMachine;

fn main() {
    #[cfg(feature = "pacing")]
    println!("pacing: ON");
    #[cfg(not(feature = "pacing"))]
    println!("pacing: OFF");
    #[cfg(feature = "path-selection")]
    println!("path-selection: ON");
    #[cfg(feature = "resume")]
    println!("resume: ON");
    // The core (sender) is always available.
    let _ = MediaSenderMachine::new;
    println!("core: always");
}
