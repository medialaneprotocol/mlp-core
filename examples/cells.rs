//! Cells example: cell grid and accounting.

use mlp_core::media::cells::CellGrid;

fn main() {
    let grid = CellGrid::with_cell_size(320, 240, 64);
    println!("cells: {}", grid.cells());
    println!("width: {}, height: {}", grid.width(), grid.height());
}
