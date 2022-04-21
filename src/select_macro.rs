/// # Select multiple selectables simultaneously for reading and writing
///
/// select! supports four different clauses:
///
/// * pattern = <-reader[, if condition] => code,
/// * pattern = writer<-expression[, if condition] => code,
/// * default => code,
/// * complete => code,
///
/// ## Restrictions
/// * `reader` and `writer` must be `mut` idents but not expressions.
/// * `pattern` must be irrefutable.
///
/// ## Evaluation
/// * All conditions are evaluated before selection.
/// * Expressions to `writer` are only evaluated if that branch is selected.
/// * `complete` case is run if all selectables are disabled or completed.
/// * `default` case is run if no selectable is ready, or all selectables are disabled or completed
/// in absent of `complete`.
///
/// ## Panics
/// * Panic when all selectables are disabled or completed and there is no `default` or `complete`.
///
/// ## Examples
/// ```rust
/// use stuck::channel::serial;
/// use stuck::select;
///
/// #[stuck::main]
/// fn main() {
///     let (mut sender, mut receiver) = serial::bounded(1);
///     let mut sent = false;
///     let mut received = false;
///     loop {
///         select! {
///             _ = <-receiver => received = true,
///             _ = sender<-1, if !sent  => sent = true,
///             default => break,
///         }
///     }
///     assert!(sent);
///     assert!(received);
/// }
/// ```
#[macro_export]
macro_rules! select {
    ($($tokens:tt)*) => {
        $crate::select_internal!(@list ($($tokens)*) ())
    }
}

// @list list branches and normalize branch body to form `{ $body; }` with trailing comma.
// @case pattern match branches and verify them
//
// @init generate initialization code
// @add prepare selectables and select for permit
// @match match selection to select branch for execution
#[doc(hidden)]
#[macro_export]
macro_rules! select_internal {
    (@list
        ()
        $cases:tt
    ) => {
        $crate::select_internal!(
            @case
            $cases
            ()
            ()
            ()
        )
    };

    // `complete` in last case.
    (@list
        (complete => $body:expr)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ()
            ($($cases)* complete => { $body; },)
         )
    };
    // `complete` in no last case.
    (@list
        (complete => $body:expr, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* complete => { $body; },)
        )
    };

    // List default case.

    // `default` in last case
    (@list
        (default => $body:expr)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ()
            ($($cases)* default => { $body; },)
        )
    };
    // `default` in no last case
    (@list
        (default => $body:expr, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* default => { $body; },)
        )
    };

    // List operation case. `block` is a special kind of `expr`, match it first.

    // recv: Block with trailing comma.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $body:block, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* recv($r) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // recv: Block without trailing comma.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $body:block $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* recv($r) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // recv: Comma is optional in last case.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $body:expr)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ()
            ($($cases)* recv($r) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // recv: Comma is mandatory in no last case.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $body:expr, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* recv($r) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // recv: Complain missing comma.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        compile_error!("expect `,` after expression in not last branch")
    };

    // send: Block with trailing comma.
    (@list
        ($bind:pat = $sender:ident <- $value:expr $(, if $pred:expr)? => $body:block, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* send($sender, $value) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // send: Block without trailing comma.
    (@list
        ($bind:pat = $sender:ident <- $value:expr $(, if $pred:expr)? => $body:block $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* send($sender, $value) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // send: Comma is optional in last case.
    (@list
        ($bind:pat = $sender:ident <- $value:expr $(, if $pred:expr)? => $body:expr)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ()
            ($($cases)* send($sender, $value) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // send: Comma is mandatory in no last case.
    (@list
        ($bind:pat = $sender:ident <- $value:expr $(, if $pred:expr)? => $body:expr, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select_internal!(
            @list
            ($($tokens)*)
            ($($cases)* send($sender, $value) -> $bind, [$($pred)*] => { $body; },)
        )
    };
    // send: Complain missing comma.
    (@list
        ($bind:pat = $sender:ident <- $value:expr $(, if $pred:expr)? => $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        compile_error!("expect `,` after expression in not last branch")
    };

    (@list
        ($($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        compile_error!("fail to list select cases")
    };

    // All cases are verified, let's generate code.
    (@case
        ()
        $branches:tt
        $default:tt
        $complete:tt
    ) => {
        $crate::select_internal!(@init $branches $default $complete)
    };

    // `default` case.
    (@case
        (default => $body:tt, $($cases:tt)*)
        $branches:tt
        ()
        $complete:tt
    ) => {
        $crate::select_internal!(
            @case
            ($($cases)*)
            $branches
            (default => $body,)
            $complete
        )
    };
    // At most one `default` case.
    (@case
        (default $($unused:tt)*)
        $branches:tt
        ($($def:tt)+)
        $complete:tt
    ) => {
        compile_error!("more than one `default` case in `select` block")
    };

    // `complete` case.
    (@case
        (complete => $body:tt, $($cases:tt)*)
        $branches:tt
        $default:tt
        ()
    ) => {
        $crate::select_internal!(
            @case
            ($($cases)*)
            $branches
            $default
            (complete => $body,)
        )
    };
    // At most one `complete` case.
    (@case
        (complete $($unused:tt)*)
        ($($branches:tt)*)
        $default:tt
        ($($tok:tt)+)
    ) => {
        compile_error!("more than one `complete` case in `select` block")
    };

    // Recv case.
    (@case
        (recv($r:ident) -> $bind:pat, $pred:tt => $body:tt, $($cases:tt)*)
        ($($branches:tt)*)
        $default:tt
        $complete:tt
    ) => {
        $crate::select_internal!(
            @case
            ($($cases)*)
            ($($branches)* recv($r) -> $bind, $pred => $body,)
            $default
            $complete
        )
    };

    // Send case.
    (@case
        (send($s:ident, $v:expr) -> $bind:pat, $pred:tt => $body:tt, $($cases:tt)*)
        ($($branches:tt)*)
        $default:tt
        $complete:tt
    ) => {
        $crate::select_internal!(
            @case
            ($($cases)*)
            ($($branches)* send($s, $v) -> $bind, $pred => $body,)
            $default
            $complete
        )
    };

    // Init select.
    (@init
        $branches:tt
        $default:tt
        $complete:tt
    ) => {{
        #[allow(unused_imports)]
        use $crate::select::Selectable as _;
        #[allow(unused_imports)]
        use $crate::select::Select as _;
        #[allow(unused_imports)]
        use $crate::select::PermitReader as _;
        #[allow(unused_imports)]
        use $crate::select::PermitWriter as _;

        const _LEN: usize = $crate::select_internal!(@count $branches);
        let mut _selector: [Option<&'_ dyn $crate::select::Selectable>; _LEN] = [::std::option::Option::None; _LEN];
        $crate::select_internal!(
            @add
            _selector
            $branches
            $default
            $complete
            (
                (0x00usize)
                (0x01usize)
                (0x02usize)
                (0x03usize)
                (0x04usize)
                (0x05usize)
                (0x06usize)
                (0x07usize)
                (0x08usize)
                (0x09usize)
                (0x0ausize)
                (0x0busize)
                (0x0cusize)
                (0x0dusize)
                (0x0eusize)
                (0x0fusize)
                (0x10usize)
                (0x11usize)
                (0x12usize)
                (0x13usize)
                (0x14usize)
                (0x15usize)
                (0x16usize)
                (0x17usize)
                (0x18usize)
                (0x19usize)
                (0x1ausize)
                (0x1busize)
                (0x1cusize)
                (0x1dusize)
                (0x1eusize)
                (0x1fusize)
            )
            ()
        )
    }};

    // Let's select!
    (@add
        $selector:ident
        ()
        ()
        ()
        $lables:tt
        $candidates:tt
    ) => {{
        if let Some(_selection) = unsafe { $selector.select() } {
            $crate::select_internal!(
                @match
                $selector
                _selection
                $candidates
            )
        } else {
            panic!("all selectables are disabled or completed and there is no `default` or `complete`");
        }
    }};

    // `select!` with `complete` branch
    (@add
        $selector:ident
        ()
        ()
        (complete => $body:tt,)
        $lables:tt
        $candidates:tt
    ) => {{
        if let Some(_selection) = unsafe { $selector.select() } {
            $crate::select_internal!(
                @match
                $selector
                _selection
                $candidates
            )
        } else {
            $body
        }
    }};

    // `select!` with `default` branch
    (@add
        $selector:ident
        ()
        (default => $body:tt,)
        ()
        $lables:tt
        $candidates:tt
    ) => {{
        let _result = unsafe {  $selector.try_select() };
        { $selector };
        match _result {
            ::std::result::Result::Err(_) => {
                $body
            },
            ::std::result::Result::Ok(_selection) => {
                $crate::select_internal!(
                    @match
                    $selector
                    _selection
                    $candidates
                )
            }
        }
    }};

    // `select!` with `default` and `complete` branch
    (@add
        $selector:ident
        ()
        (default => $default:tt,)
        (complete => $complete:tt,)
        $lables:tt
        $candidates:tt
    ) => {{
        use $crate::select::TrySelectError;
        let _result = unsafe {  $selector.try_select() };
        { $selector };
        match _result {
            ::std::result::Result::Err(TrySelectError::Completed) => {
                $complete
            },
            ::std::result::Result::Err(TrySelectError::WouldBlock) => {
                $default
            },
            ::std::result::Result::Ok(_selection) => {
                $crate::select_internal!(
                    @match
                    $selector
                    _selection
                    $candidates
                )
            }
        }
    }};

    // No remaining labels.
    (@add
        $selector:ident
        $branches:tt
        $default:tt
        $complete:tt
        ()
        $candidates:tt
    ) => {
        compile_error!("too many select! cases")
    };

    // Select receiver.
    (@add
        $selector:ident
        (recv($r:ident) -> $bind:pat, [$($pred:expr)?] => $body:tt, $($branches:tt)*)
        $default:tt
        $complete:tt
        (($index:tt) $($labels:tt)*)
        ($($candidates:tt)*)
    ) => {{
        let mut _enabled = true;
        $( _enabled = $pred; ) ?
        if _enabled {
            let ref _ref = $r;
            let _ref = unsafe { ::std::mem::transmute::<&dyn $crate::select::Selectable, &'_ dyn $crate::select::Selectable>(_ref) };
            $selector[$index] = Some(_ref);
        }
        $crate::select_internal!(
            @add
            $selector
            ($($branches)*)
            $default
            $complete
            ($($labels)*)
            ($($candidates)* [$index] recv($r) -> $bind => $body,)
        )
    }};

    // Select sender.
    (@add
        $selector:ident
        (send($s:ident, $v:expr) -> $bind:pat, [$($pred:expr)?] => $body:tt, $($branches:tt)*)
        $default:tt
        $complete:tt
        (($index:tt) $($labels:tt)*)
        ($($candidates:tt)*)
    ) => {{
        let mut _enabled = true;
        $( _enabled = $pred; ) ?
        if _enabled {
            let ref _ref = $s;
            let _ref = unsafe { ::std::mem::transmute::<&dyn $crate::select::Selectable, &'_ dyn $crate::select::Selectable>(_ref) };
            $selector[$index] = Some(_ref);
        }
        $crate::select_internal!(
            @add
            $selector
            ($($branches)*)
            $default
            $complete
            ($($labels)*)
            ($($candidates)* [$index] send($s, $v) -> $bind => $body,)
        )
    }};

    // Panic if no matching selectable.
    (@match
        $selector:ident
        $selection:ident
        ()
    ) => {{
        unreachable!("no matching selectable")
    }};

    // Match a receive operation.
    (@match
        $selector:ident
        $selection:ident
        ([$index:tt] recv($r:ident) -> $bind:pat => $body:tt, $($candidates:tt)*)
    ) => {{
        if $selection.0 == $index {
            { $selector };
            let $bind = $r.consume_permit($selection.1);
            #[allow(unreachable_code)]
            $body
        } else {
            $crate::select_internal!(
                @match
                $selector
                $selection
                ($($candidates)*)
            )
        }
    }};

    // Match a send operation.
    (@match
        $selector:ident
        $selection:ident
        ([$index:tt] send($s:ident, $v:expr) -> $bind:pat => $body:tt, $($candidates:tt)*)
    ) => {{
        if $selection.0 == $index {
            { $selector };
            let $bind = $s.consume_permit($selection.1, $v);
            #[allow(unreachable_code)]
            $body
        } else {
            $crate::select_internal!(
                @match
                $selector
                $selection
                ($($candidates)*)
            )
        }
    }};

    // Count select cases.
    (@count ()) => { 0 };
    (@count ($ident:ident $args:tt -> $bind:pat, $pred:tt => $body:tt, $($cases:tt)*)) => {
        1 + $crate::select_internal!(@count ($($cases)*))
    };

    // Entry points.
    () => {
        compile_error!("empty `select!` block")
    };
    ($($tokens:tt)*) => {
        $crate::select_internal!(@list ($($tokens)*) ())
    }
}
