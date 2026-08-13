import sys, json, io, traceback, base64, ast, linecache

_ns = {'__name__': '__main__'}
_real_stdout = sys.stdout

# The id of the request currently being served.  Stamped onto every message so
# the editor can route a reply to whoever asked for it, rather than inferring
# the target from what it last sent.
_req_id = 0

def _emit(obj):
    obj['id'] = _req_id
    _real_stdout.write(json.dumps(obj) + '\n')
    _real_stdout.flush()

def _emit_png_bytes(raw):
    _emit({'t': 'image', 'data': base64.b64encode(raw).decode('ascii')})

# Rasterise a LaTeX string (e.g. SymPy's _repr_latex_) to a PNG via matplotlib's
# mathtext engine, then emit it through the normal image channel.  mathtext
# supports a wide subset of LaTeX (fractions, powers, sqrt, greek, sums…), which
# covers typical SymPy output.  Returns True on success; the caller falls back
# to a plain-text repr when this returns False (no matplotlib, or unsupported
# markup).
def _emit_latex(latex):
    if not _capture_matplotlib:
        return False
    try:
        import matplotlib.pyplot as _plt
        s = latex.strip()
        # Strip surrounding $ / $$ delimiters and \displaystyle (unsupported).
        while s.startswith('$'):
            s = s[1:]
        while s.endswith('$'):
            s = s[:-1]
        s = s.replace('\\displaystyle', '').strip()
        if not s:
            return False
        expr = '$' + s + '$'
        _fig = _plt.figure()
        _fig.patch.set_facecolor('white')
        _fig.text(0.5, 0.5, expr, fontsize=18, ha='center', va='center', color='black')
        _buf = io.BytesIO()
        _fig.savefig(_buf, format='png', dpi=150, bbox_inches='tight', facecolor='white')
        _plt.close(_fig)
        _buf.seek(0)
        _emit_png_bytes(_buf.read())
        return True
    except Exception:
        return False

# Display the value of a cell's trailing expression, preferring rich reprs
# (LaTeX → PNG, then _repr_png_) and falling back to repr().
def _display_result(obj):
    try:
        m = getattr(obj, '_repr_latex_', None)
        latex = m() if callable(m) else None
    except Exception:
        latex = None
    if latex and _emit_latex(latex):
        return
    try:
        m = getattr(obj, '_repr_png_', None)
        png = m() if callable(m) else None
        if png:
            raw = png if isinstance(png, (bytes, bytearray)) else base64.b64decode(png)
            _emit_png_bytes(raw)
            return
    except Exception:
        pass
    _emit({'t': 'stream', 'name': 'stdout', 'text': repr(obj) + '\n'})

# At startup, try to configure matplotlib with the Agg (non-interactive) backend
# so that plt.show() captures figures without requiring %matplotlib inline.
_capture_matplotlib = False
try:
    import matplotlib as _mpl
    try:
        _mpl.use('Agg', force=True)
    except TypeError:
        _mpl.use('Agg')
    import matplotlib.pyplot as _plt_global
    _capture_matplotlib = True
    # We capture figures via savefig(); make show() a no-op so it doesn't
    # emit "FigureCanvasAgg is non-interactive" warnings on every plt.show() call.
    _plt_global.show = lambda **kw: None
except Exception:
    pass

class _Fwd(io.TextIOBase):
    def __init__(self, name):
        self._name = name
    def writable(self):
        return True
    def write(self, s):
        if s:
            _emit({'t': 'stream', 'name': self._name, 'text': s})
        return len(s)
    def flush(self):
        pass

_real_stdout.write('__KI_READY__\n')
_real_stdout.flush()

while True:
    lines = []
    for line in sys.stdin:
        s = line.rstrip('\n')
        if s == '__KI_CODE_END__':
            break
        lines.append(s)
    else:
        break  # stdin closed — kernel shutting down

    # The editor prefixes a control line: __KI_REQ__ followed by a JSON header.
    #
    #   id   — echoed on every message of the reply, so the editor knows who
    #          asked.  Requests are served strictly in order, one at a time.
    #   kind — what to do.  Only 'exec' today; the field exists so a new kind
    #          can be added without changing the framing.
    #   tag  — the cell's stable id.  We compile under that name so tracebacks
    #          report `File "<id>", line N` and the editor can map the frame
    #          back to a jump target.
    #
    # The header line is stripped before the code, so line numbers stay 1-based
    # to the cell source.
    _req_id = 0
    kind = 'exec'
    cell_name = '<cell>'
    if lines and lines[0].startswith('__KI_REQ__'):
        try:
            hdr = json.loads(lines.pop(0)[len('__KI_REQ__'):])
        except Exception:
            hdr = {}
        _req_id = hdr.get('id') or 0
        kind = hdr.get('kind') or 'exec'
        if hdr.get('tag'):
            cell_name = hdr['tag']

    # An unknown kind is answered rather than ignored: a newer editor talking to
    # an older kernel must get a reply, or it waits forever for one.
    if kind != 'exec':
        _emit({'t': 'error', 'text': 'kernel does not support request kind ' + repr(kind)})
        _emit({'t': 'done'})
        continue

    # Handle IPython-style line magics. Only %matplotlib is processed;
    # everything else starting with % or ! is silently dropped for now.
    code_lines = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith('%matplotlib'):
            parts = stripped.split()
            backend = parts[1] if len(parts) > 1 else 'inline'
            try:
                import matplotlib as _mpl
                if backend in ('inline', 'agg', 'Agg'):
                    try:
                        _mpl.use('Agg', force=True)
                    except TypeError:
                        _mpl.use('Agg')
                    _capture_matplotlib = True
                else:
                    try:
                        _mpl.use(backend, force=True)
                    except TypeError:
                        _mpl.use(backend)
                    _capture_matplotlib = False
            except Exception:
                pass
        elif stripped.startswith('%') or stripped.startswith('!'):
            pass  # other magics/shell escapes ignored
        else:
            code_lines.append(line)

    code = '\n'.join(code_lines)

    # Register the cell source with linecache so tracebacks show the offending
    # source line inline (linecache can't read our synthetic `<id>` filename off
    # disk). mtime None keeps the entry pinned across checkcache() calls.
    if code:
        linecache.cache[cell_name] = (len(code), None, code.splitlines(keepends=True), cell_name)

    sys.stdout, sys.stderr = _Fwd('stdout'), _Fwd('stderr')
    try:
        if code.strip():
            # Split off a trailing bare expression so its value can be displayed
            # (rich repr if available), mirroring Jupyter's execute_result.
            _parsed = ast.parse(code, cell_name, 'exec')
            _last_expr = None
            if _parsed.body and isinstance(_parsed.body[-1], ast.Expr):
                _last_expr = _parsed.body.pop()
            if _parsed.body:
                exec(compile(_parsed, cell_name, 'exec'), _ns)
            if _last_expr is not None:
                _expr_ast = ast.fix_missing_locations(ast.Expression(_last_expr.value))
                _value = eval(compile(_expr_ast, cell_name, 'eval'), _ns)
                if _value is not None:
                    _display_result(_value)
        if _capture_matplotlib:
            try:
                import matplotlib.pyplot as _plt
                fignums = _plt.get_fignums()
                for _num in fignums:
                    _fig = _plt.figure(_num)
                    _buf = io.BytesIO()
                    # No bbox_inches='tight' — preserve the figsize aspect ratio exactly.
                    _fig.savefig(_buf, format='png', dpi=150)
                    _buf.seek(0)
                    _emit({'t': 'image', 'data': base64.b64encode(_buf.read()).decode('ascii')})
                if fignums:
                    _plt.close('all')
            except Exception:
                pass
    except SystemExit:
        pass
    except BaseException:
        _emit({'t': 'error', 'text': traceback.format_exc()})
    finally:
        sys.stdout, sys.stderr = _real_stdout, sys.__stderr__

    _emit({'t': 'done'})
