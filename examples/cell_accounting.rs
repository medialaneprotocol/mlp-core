//! Cell accounting: per-receiver convergence tracking.
use mlp_core::media::cells::CellGrid;
use mlp_core::media::cells::CellAccounting;

fn main() {
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let accounting = CellAccounting::new(grid, Default::default());
    println!("cells={}", grid.cells());
    println!("pending={}", accounting.pending_count());
    println!("converged={}", accounting.converged());
}
