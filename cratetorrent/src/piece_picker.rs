use bitvec::prelude::*;
use crate::PieceIndex;

/// Specialized bitfield for tracking piece state, radiation-hardened for mission-critical operations
pub(crate) type Bitfield = BitVec<usize, Msb0>;

/// Manages piece selection strategy for torrent downloads with bounded execution guarantees
pub(crate) struct PiecePicker {
    /// Represents the pieces that we have downloaded.
    ///
    /// The bitfield is pre-allocated to the number of pieces in the torrent and
    /// each field that we have is set to true.
    own_pieces: Bitfield,

    /// We collect metadata about pieces in the torrent swarm in this vector.
    ///
    /// The vector is pre-allocated to the number of pieces in the torrent.
    pieces: Vec<Piece>,

    /// A cache for the number of pieces we haven't received yet (but may have
    /// picked).
    missing_count: usize,

    /// A cache for the number of pieces that can be picked.
    free_count: usize,
}

/// Metadata about a piece relevant for the piece picker.
#[derive(Clone, Copy, Default)]
pub(crate) struct Piece {
    /// The frequency of this piece in the torrent swarm.
    pub frequency: usize,

    /// Whether we have already picked this piece and are currently downloading
    /// it. This flag is set to true when the piece is picked.
    ///
    /// This prevents picking the same piece multiple times during concurrent downloads.
    pub is_pending: bool,
}

impl PiecePicker {
    /// Creates a new piece picker with the given own_pieces we already have.
    pub fn new(own_pieces: Bitfield) -> Self {
        // Ensure we have a valid bitfield
        debug_assert!(!own_pieces.is_empty(), "Piece count must be greater than zero");

        let piece_count = own_pieces.len();
        let mut pieces = Vec::with_capacity(piece_count);
        pieces.resize_with(piece_count, Piece::default);

        let missing_count = own_pieces.count_zeros();

        Self {
            own_pieces,
            pieces,
            missing_count,
            free_count: missing_count,
        }
    }

    /// Returns an immutable reference to a bitfield of the pieces we own.
    pub fn own_pieces(&self) -> &Bitfield {
        &self.own_pieces
    }

    /// Returns the number of missing pieces that are needed to complete the
    /// download.
    pub fn missing_piece_count(&self) -> usize {
        self.missing_count
    }

    /// Returns true if all pieces have been picked (whether pending or
    /// received).
    pub fn all_pieces_picked(&self) -> bool {
        self.free_count == 0
    }

    /// Returns the first piece that we don't yet have and isn't already being
    /// downloaded, or None, if no piece can be picked at this time.
    pub fn pick_piece(&mut self) -> Option<PieceIndex> {
        log::trace!("Picking next piece");

        for index in 0..self.own_pieces.len() {
            // only consider this piece if we don't have it, it's available from peers,
            // and we are not already downloading it
            debug_assert!(index < self.pieces.len(), "Piece index out of bounds");
            let piece = &mut self.pieces[index];
            if !self.own_pieces[index]
                && piece.frequency > 0
                && !piece.is_pending
            {
                // set pending flag on piece so that this piece is not picked
                // again (see note on field)
                piece.is_pending = true;
                self.free_count = self.free_count.saturating_sub(1);
                log::trace!("Picked piece {}", index);
                return Some(index);
            }
        }

        // no piece could be picked
        log::trace!("Could not pick piece");
        None
    }

    /// Registers the availability of a peer's pieces and returns whether we're
    /// interested in peer's pieces.
    ///
    /// # Panics
    ///
    /// Panics if the peer sent us pieces with a different count than ours.
    pub fn register_peer_pieces(&mut self, pieces: &Bitfield) -> bool {
        log::trace!("Registering piece availability: bitfield of length {}", pieces.len());

        assert_eq!(
            pieces.len(),
            self.own_pieces.len(),
            "peer's bitfield must be the same length as ours"
        );

        let mut interested = false;
        for (index, have_peer_piece) in pieces.iter().enumerate() {
            // increase frequency count for this piece if peer has it
            if *have_peer_piece {
                debug_assert!(index < self.pieces.len(), "Piece index out of bounds");
                self.pieces[index].frequency = self.pieces[index].frequency.saturating_add(1);

                // if we don't have at least one piece peer has, we're interested
                if !self.own_pieces[index] {
                    interested = true;
                }
            }
        }

        interested
    }

    /// Increments the availability of a piece.
    ///
    /// This should be called when a peer sends us a `have` message of a new
    /// piece.
    ///
    /// # Returns
    ///
    /// Returns true if we're interested in this piece (don't have it yet)
    ///
    /// # Panics
    ///
    /// Panics if the piece index is out of range.
    pub fn register_peer_piece(&mut self, index: PieceIndex) -> bool {
        log::trace!("Registering newly available piece {}", index);

        // Bounds checking with detailed error message for debugging
        assert!(index < self.own_pieces.len(),
                "Invalid piece index: {} (max: {})",
                index,
                self.own_pieces.len() - 1);

        let have_piece = self.own_pieces[index];
        self.pieces[index].frequency = self.pieces[index].frequency.saturating_add(1);
        !have_piece // Return true if we don't have the piece (and thus interested)
    }

    /// Tells the piece picker that we have downloaded the piece at the given
    /// index that we had picked before.
    ///
    /// # Panics
    ///
    /// Panics if the piece was already received.
    pub fn received_piece(&mut self, index: PieceIndex) {
        log::trace!("Registering received piece {}", index);

        // Validate index is within bounds
        assert!(index < self.own_pieces.len(),
                "Invalid piece index: {} (max: {})",
                index,
                self.own_pieces.len() - 1);

        // We must not already have this piece
        assert!(!self.own_pieces[index],
                "Piece {} was already received",
                index);

        // Register owned piece
        self.own_pieces.set(index, true);
        self.missing_count = self.missing_count.saturating_sub(1);

        // Handle edge case: if the piece was received without being picked first
        let piece = &mut self.pieces[index];
        if !piece.is_pending {
            self.free_count = self.free_count.saturating_sub(1);
        }

        // Reset pending flag for potential future re-downloads (e.g., after piece verification failure)
        piece.is_pending = false;
    }

    /// Access to piece metadata for diagnostic and statistics purposes
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Creates a piece picker with no owned pieces
    #[cfg(test)]
    fn empty(piece_count: usize) -> Self {
        Self::new(BitVec::repeat(false, piece_count))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use super::*;

    /// Tests that repeatedly requesting as many pieces as are in the piece
    /// picker returns all pieces, none of them previously picked.
    #[test]
    fn should_pick_all_pieces() {
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);
        let available_pieces = BitVec::repeat(true, piece_count);
        piece_picker.register_peer_pieces(&available_pieces);

        // save picked pieces
        let mut picked = HashSet::with_capacity(piece_count);

        // pick all pieces one by one
        for index in 0..piece_count {
            let pick = piece_picker.pick_piece();
            // for now we assert that we pick pieces in sequential order, but
            // later, when we add different algorithms, this line has to change
            assert_eq!(pick, Some(index));
            let pick = pick.unwrap();
            // assert that this piece hasn't been picked before
            assert!(!picked.contains(&pick));
            // mark piece as picked
            picked.insert(pick);
        }

        // assert that we picked all pieces
        assert_eq!(picked.len(), piece_count);
    }

    /// Tests registering a received piece causes the piece picker to not pick
    /// that piece again.
    #[test]
    fn should_mark_piece_as_received() {
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);
        let available_pieces = BitVec::repeat(true, piece_count);
        piece_picker.register_peer_pieces(&available_pieces);
        assert!(piece_picker.own_pieces.not_any());

        // mark pieces as received
        let owned_pieces = [3, 10, 5];
        for index in owned_pieces.iter() {
            piece_picker.received_piece(*index);
            assert!(piece_picker.own_pieces[*index]);
        }
        assert!(!piece_picker.own_pieces.not_any());

        // request pieces to pick next and make sure the ones we already have
        // are not picked
        for _ in 0..piece_count - owned_pieces.len() {
            let pick = piece_picker.pick_piece().unwrap();
            // assert that it's not a piece we already have
            assert!(owned_pieces.iter().all(|owned| *owned != pick));
        }
    }

    #[test]
    fn should_count_missing_pieces() {
        // empty piece picker
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);

        assert_eq!(piece_picker.missing_piece_count(), piece_count);

        // set 2 pieces
        let have_count = 2;
        for index in 0..have_count {
            piece_picker.received_piece(index);
        }
        assert_eq!(
            piece_picker.missing_piece_count(),
            piece_count - have_count
        );

        // set all pieces
        for index in have_count..piece_count {
            piece_picker.received_piece(index);
        }
        assert_eq!(piece_picker.missing_piece_count(), 0);
    }

    /// Tests that the piece picker correctly reports pieces that were not
    /// picked or received.
    #[test]
    fn should_count_free_pieces() {
        // empty piece picker
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);
        // NOTE: need to register frequency before we pick any pieces
        piece_picker.register_peer_pieces(&BitVec::repeat(true, piece_count));

        assert_eq!(piece_picker.free_count, piece_count);

        // picked and received 2 pieces
        for i in 0..2 {
            assert!(piece_picker.pick_piece().is_some());
            piece_picker.received_piece(i);
        }
        assert_eq!(piece_picker.free_count, 13);

        // pick 3 pieces
        for _ in 0..3 {
            assert!(piece_picker.pick_piece().is_some());
        }
        assert_eq!(piece_picker.free_count, 10);

        // received 1 of the above picked pieces: shouldn't change outcome
        piece_picker.received_piece(2);
        assert_eq!(piece_picker.free_count, 10);

        // pick rest of the pieces
        for _ in 0..10 {
            assert!(piece_picker.pick_piece().is_some());
        }
        assert!(piece_picker.all_pieces_picked());
    }

    /// Tests that the piece picker correctly determines whether we are
    /// interested in a variety of piece sets.
    #[test]
    fn should_determine_interest() {
        // empty piece picker
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);

        // we are interested if peer has all pieces
        let available_pieces = BitVec::repeat(true, piece_count);
        assert!(piece_picker.register_peer_pieces(&available_pieces));

        // we are also interested if peer has at least a single piece
        let mut available_pieces = BitVec::repeat(false, piece_count);
        available_pieces.set(0, true);
        assert!(piece_picker.register_peer_pieces(&available_pieces));

        // half full piece picker
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);
        for index in 0..8 {
            piece_picker.received_piece(index);
        }

        // we are not interested in peer that has the same pieces we do
        let mut available_pieces = BitVec::repeat(false, piece_count);
        for index in 0..8 {
            available_pieces.set(index, true);
        }
        assert!(!piece_picker.register_peer_pieces(&available_pieces));

        // we are interested in peer that has at least a single piece we don't
        let mut available_pieces = BitVec::repeat(false, piece_count);
        for index in 0..9 {
            available_pieces.set(index, true);
        }
        assert!(piece_picker.register_peer_pieces(&available_pieces));

        // full piece picker
        let piece_count = 15;
        let mut piece_picker = PiecePicker::empty(piece_count);
        for index in 0..piece_count {
            piece_picker.received_piece(index);
        }

        // we are not interested in any pieces since we own all of them
        assert!(!piece_picker.register_peer_pieces(&available_pieces));
    }
}
