/// Select multiple channels simultaneously for sending and receiving.
///
/// select! supports three different clauses for send, receive and default respectively.
///
/// * pattern = <-receiver[, if condition] => code,
/// * pattern = sender<-expression[, if condition] => code,
/// * default => code,
///
/// ## Restrictions
/// * `sender` and `receiver` must be `mut` idents but not expressions.
/// * `pattern` must be irrefutable.
///
/// ## Evaluation
/// * All conditions are evaluated before selection.
/// * Send expression is only evaluated if that branch is selected.
/// * `default` case is run if no other case is ready.
///
/// ## Panics
/// * Panic when all send and receive branches are disabled and there is no default case.
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

// @list list cases and normalize branch body to form `{ $body; }` with trailing comma.
// @case pattern match cases and verify them
//
// @init generate initialization code
// @add prepare selectables and select for permit
// @complete match selection to select branch for execution
#[macro_export]
macro_rules! select {
    (@list
        ()
        $cases:tt
    ) => {
        $crate::select!(
            @case
            $cases
            ()
            ()
        )
    };

    // List default case.

    // Last clause.
    (@list
        (default => $body:expr)
        ($($cases:tt)*)
    ) => {
        $crate::select!(
            @list
            ()
            ($($cases)* default => { $body; },)
        )
    };
    // No last clause.
    (@list
        (default => $body:expr, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select!(
            @list
            ($($tokens)*)
            ($($cases)* default => { $body; },)
        )
    };
    // // Expect `=>` after `default`.
    // (@list
    //     (default -> $($tokens:tt)*)
    //     $cases:tt
    // ) => {
    //     compile_error!("expect `=>` after `default`, got `->`")
    // };

    // // Binding is mandatory.
    // (@list
    //     (recv($($args:tt)*) => $($unused:tt)*)
    //     ($($cases:tt)*)
    // ) => {
    //     compile_error!("expect `->` after `recv`, got `=>`")
    // };
    // (@list
    //     (send($($args:tt)*) => $($unused:tt)*)
    //     ($($cases:tt)*)
    // ) => {
    //     compile_error!("expect `->` after `send`, got `=>`")
    // };

    // List operation case. `block` is a special kind of `expr`, match it first.

    // recv: Block with trailing comma.
    (@list
        ($bind:pat = <- $r:ident $(, if $pred:expr)? => $body:block, $($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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
        $crate::select!(
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

    // // Block with trailing comma.
    // (@list
    //     ($bind:pat = $operation:ident($($args:tt)*) $(, if $pred:expr)? => $body:block, $($tokens:tt)*)
    //     ($($cases:tt)*)
    // ) => {
    //     $crate::select!(
    //         @list
    //         ($($tokens)*)
    //         ($($cases)* $operation($($args)*) -> $bind, [$($pred)*] => { $body },)
    //     )
    // };
    // // Block without trailing comma.
    // (@list
    //     ($bind:pat = $operation:ident($($args:tt)*) $(, if $pred:expr)? => $body:block $($tokens:tt)*)
    //     ($($cases:tt)*)
    // ) => {
    //     $crate::select!(
    //         @list
    //         ($($tokens)*)
    //         ($($cases)* $operation($($args)*) -> $bind, [$($pred)*] => { $body },)
    //     )
    // };
    // // Comma is optional in last case.
    // (@list
    //     ($bind:pat = $operation:ident($($args:tt)*) $(, if $pred:expr)? => $body:expr)
    //     ($($cases:tt)*)
    // ) => {
    //     $crate::select!(
    //         @list
    //         ()
    //         ($($cases)* $operation($($args)*) -> $bind, [$($pred)?] => { $body },)
    //     )
    // };
    // // Comma is mandatory in no last case.
    // (@list
    //     ($bind:pat = $operation:ident($($args:tt)*) $(, if $pred:expr)? => $body:expr, $($tokens:tt)*)
    //     ($($cases:tt)*)
    // ) => {
    //     $crate::select!(
    //         @list
    //         ($($tokens)*)
    //         ($($cases)* $operation($($args)*) -> $bind, [$($pred)*] => { $body },)
    //     )
    // };

    (@list
        ($($tokens:tt)*)
        ($($cases:tt)*)
    ) => {
        compile_error!("fail to list select cases")
    };

    // All cases are verified, let's generate code.
    (@case
        ()
        $cases:tt
        $default:tt
    ) => {
        $crate::select!(@init $cases $default)
    };

    // `default` case.
    (@case
        (default => $body:tt, $($pendings:tt)*)
        ($($cases:tt)*)
        ()
    ) => {
        $crate::select!(
            @case
            ($($pendings)*)
            ($($cases)*)
            (default => $body,)
        )
    };
    // At most one default case.
    (@case
        (default $($unused:tt)*)
        ($($cases:tt)*)
        ($($def:tt)+)
    ) => {
        compile_error!("more than one `default` case in `select` block")
    };

    // Recv case.
    (@case
        (recv($r:ident) -> $bind:pat, $pred:tt => $body:tt, $($pendings:tt)*)
        ($($cases:tt)*)
        $default:tt
    ) => {
        $crate::select!(
            @case
            ($($pendings)*)
            ($($cases)* recv($r) -> $bind, $pred => $body,)
            $default
        )
    };
    // // `recv` requires exact one argument.
    // (@case
    //     (recv($($args:tt)*) -> $bind:pat, $pred:tt => $body:tt, $($pendings:tt)*)
    //     $cases:tt
    //     $default:tt
    // ) => {
    //     compile_error!(concat!(
    //         "expect one argument for `recv`, got: ",
    //         stringify!($($args)*)
    //     ))
    // };

    // Send case.
    (@case
        (send($s:ident, $v:expr) -> $bind:pat, $pred:tt => $body:tt, $($pendings:tt)*)
        ($($cases:tt)*)
        $default:tt
    ) => {
        $crate::select!(
            @case
            ($($pendings)*)
            ($($cases)* send($s, $v) -> $bind, $pred => $body,)
            $default
        )
    };
    // // `send` requires exact two arguments.
    // (@case
    //     (send($($args:tt)*) -> $bind:pat, $pred:tt => $body:tt, $($pendings:tt)*)
    //     $cases:tt
    //     $default:tt
    // ) => {
    //     compile_error!(concat!("expect two argument for `send`, got: ", stringify!($($args)*)))
    // };

    // // Unknown operation.
    // (@case
    //     ($operation:ident $($tokens:tt)*)
    //     $cases:tt
    //     $default:tt
    // ) => {
    //     compile_error!(concat!("expect `recv` or `send`, got `", stringify!($operation), "`"))
    // };

    // Init select.
    (@init
        $cases:tt
        $default:tt
    ) => {{
        use $crate::channel::select::Select;
        const _LEN: usize = $crate::select!(@count $cases);
        let mut _selector: [Option<&'_ dyn $crate::channel::select::Selectable>; _LEN] = [::std::option::Option::None; _LEN];
        $crate::select!(
            @add
            _selector
            $cases
            $default
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
        $lables:tt
        $cases:tt
    ) => {{
        let _selection = unsafe { $selector.select() };
        $crate::select!(
            @complete
            $selector
            _selection
            $cases
        )
    }};

    // Try select!
    (@add
        $selector:ident
        ()
        (default => $body:tt,)
        $lables:tt
        $cases:tt
    ) => {{
        let _selection = unsafe {  $selector.try_select() };
        match _selection {
            ::std::option::Option::None => {
                { $selector };
                $body
            },
            ::std::option::Option::Some(_selection) => {
                $crate::select!(
                    @complete
                    $selector
                    _selection
                    $cases
                )
            }
        }
    }};

    // No remaining labels.
    (@add
        $selector:ident
        $candidates:tt
        $default:tt
        ()
        $cases:tt
    ) => {
        compile_error!("too many select! cases")
    };

    // Select receiver.
    (@add
        $selector:ident
        (recv($r:ident) -> $bind:pat, [$($pred:expr)?] => $body:tt, $($pendings:tt)*)
        $default:tt
        (($index:tt) $($labels:tt)*)
        ($($cases:tt)*)
    ) => {{
        let mut _enabled = true;
        $( _enabled = $pred; ) ?
        if _enabled {
            let ref _ref = $r;
            let _ref = unsafe { ::std::mem::transmute::<&dyn $crate::channel::select::Selectable, &'_ dyn $crate::channel::select::Selectable>(_ref) };
            $selector[$index] = Some(_ref);
        }
        $crate::select!(
            @add
            $selector
            ($($pendings)*)
            $default
            ($($labels)*)
            ($($cases)* [$index] recv($r) -> $bind => $body,)
        )
    }};

    // Select sender.
    (@add
        $selector:ident
        (send($s:ident, $v:expr) -> $bind:pat, [$($pred:expr)?] => $body:tt, $($pendings:tt)*)
        $default:tt
        (($index:tt) $($labels:tt)*)
        ($($cases:tt)*)
    ) => {{
        let mut _enabled = true;
        $( _enabled = $pred; ) ?
        if _enabled {
            let ref _ref = $s;
            let _ref = unsafe { ::std::mem::transmute::<&dyn $crate::channel::select::Selectable, &'_ dyn $crate::channel::select::Selectable>(_ref) };
            $selector[$index] = Some(_ref);
        }
        $crate::select!(
            @add
            $selector
            ($($pendings)*)
            $default
            ($($labels)*)
            ($($cases)* [$index] send($s, $v) -> $bind => $body,)
        )
    }};

    // Panic if no matching selectable.
    (@complete
        $selector:ident
        $selection:ident
        ()
    ) => {{
        unreachable!("no matching selectable")
    }};

    // Match a receive operation.
    (@complete
        $selector:ident
        $selection:ident
        ([$index:tt] recv($r:ident) -> $bind:pat => $body:tt, $($cases:tt)*)
    ) => {{
        if $selection.0 == $index {
            { $selector };
            use $crate::channel::select::PermitReceiver as _;
            let $bind = $r.consume_permit($selection.1);
            $body
        } else {
            $crate::select!(
                @complete
                $selector
                $selection
                ($($cases)*)
            )
        }
    }};

    // Match a send operation.
    (@complete
        $selector:ident
        $selection:ident
        ([$index:tt] send($s:ident, $v:expr) -> $bind:pat => $body:tt, $($cases:tt)*)
    ) => {{
        if $selection.0 == $index {
            { $selector };
            use $crate::channel::select::PermitSender as _;
            let $bind = $s.consume_permit($selection.1, $v);
            $body
        } else {
            $crate::select!(
                @complete
                $selector
                $selection
                ($($cases)*)
            )
        }
    }};

    // Count select cases.
    (@count ()) => { 0 };
    (@count ($ident:ident $args:tt -> $bind:pat, $pred:tt => $body:tt, $($cases:tt)*)) => {
        1 + $crate::select!(@count ($($cases)*))
    };

    // Entry points.
    () => {
        compile_error!("empty `select!` block")
    };
    ($($tokens:tt)*) => {
        $crate::select!(@list ($($tokens)*) ())
    }
}
