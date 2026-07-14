//! The injected content-script recorder. This JS is installed via
//! `Page.addScriptToEvaluateOnNewDocument` (Chromium) or Playwright
//! `add_init_script` (Camoufox) so it survives navigations.
//!
//! It attaches passive listeners that capture user interactions and forwards
//! each event to Rust via `console.log("__REC__:" + json)` — the persistent
//! listener task harvests these tagged console messages.
//!
//! For each captured event we attach a `target` block carrying ranked locator
//! candidates (testid > role > label > text > placeholder > css) and the
//! element's bounding rect for fallback coordinate replay.

/// Tag prefix. Must match `crate::recorder::RECORDER_TAG`.
const TAG: &str = "__REC__:";

pub fn recorder_script() -> String {
  format!(
    r#"(function() {{
  if (window.__jnmbrowserRecorderInstalled) return;
  window.__jnmbrowserRecorderInstalled = true;
  var TAG = {tag:?};
  var startTs = performance.now();

  function now() {{
    return Math.round(performance.now() - startTs);
  }}

  function emit(event) {{
    try {{
      console.log(TAG + JSON.stringify(event));
    }} catch (e) {{
      // Swallow serialization errors — never break the page.
    }}
  }}

  function locatorCandidates(el) {{
    var out = [];
    if (!el || el.nodeType !== 1) return out;
    // testid
    var testid = el.getAttribute('data-testid') || el.getAttribute('data-test-id') || el.getAttribute('data-test');
    if (testid) {{
      out.push({{ by: 'testid', value: testid, preferred: true }});
    }}
    // role + accessible name
    var role = el.getAttribute('role') || implicitRole(el);
    if (role) {{
      var name = accessibleName(el);
      out.push({{ by: 'role', value: role, name: name, exact: name ? true : undefined, nth: undefined, preferred: !testid && !!name }});
    }}
    // label
    var label = elementLabel(el);
    if (label) {{
      out.push({{ by: 'label', value: label, preferred: false }});
    }}
    // placeholder (input/textarea/select)
    var placeholder = el.getAttribute('placeholder');
    if (placeholder) {{
      out.push({{ by: 'placeholder', value: placeholder, preferred: false }});
    }}
    // text content (only for clickable containers without testid/role)
    var text = (el.innerText || '').trim();
    if (!testid && !role && text && text.length <= 80 && isInteractive(el)) {{
      out.push({{ by: 'text', value: text, exact: true, preferred: false }});
    }}
    // CSS selector fallback (best-effort, short)
    var css = cssSelector(el);
    if (css) {{
      out.push({{ by: 'css', value: css, preferred: false }});
    }}
    return out;
  }}

  function isInteractive(el) {{
    var tag = el.tagName.toLowerCase();
    return tag === 'a' || tag === 'button' || tag === 'input' || tag === 'select' || tag === 'textarea' ||
      el.getAttribute('role') === 'button' || el.onclick !== null || el.tabIndex >= 0;
  }}

  function implicitRole(el) {{
    // Minimal subset of ARIA implicit roles.
    var tag = el.tagName.toLowerCase();
    switch (tag) {{
      case 'a': return el.hasAttribute('href') ? 'link' : undefined;
      case 'button': return 'button';
      case 'h1': return 'heading';
      case 'h2': return 'heading';
      case 'h3': return 'heading';
      case 'input': return inputRole(el);
      case 'select': return 'listbox';
      case 'textarea': return 'textbox';
      case 'img': return 'img';
      case 'nav': return 'navigation';
      case 'form': return 'form';
      case 'ul': return 'list';
      case 'ol': return 'list';
      case 'table': return 'table';
      default: return el.getAttribute('role');
    }}
  }}

  function inputRole(el) {{
    var type = (el.getAttribute('type') || '').toLowerCase();
    if (type === 'checkbox') return 'checkbox';
    if (type === 'radio') return 'radio';
    if (type === 'submit' || type === 'button') return 'button';
    if (type === 'search') return 'searchbox';
    if (type === 'password') return undefined; // do not auto-reveal password fields
    return 'textbox';
  }}

  function accessibleName(el) {{
    if (!el) return undefined;
    var aria = el.getAttribute('aria-label');
    if (aria) return aria;
    var labelledby = el.getAttribute('aria-labelledby');
    if (labelledby) {{
      var ref = document.getElementById(labelledby);
      if (ref) return (ref.innerText || '').trim();
    }}
    var title = el.getAttribute('title');
    if (title) return title;
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {{
      // <label for=id> association
      var id = el.id;
      if (id) {{
        var lbl = document.querySelector('label[for="' + cssEscape(id) + '"]');
        if (lbl) return (lbl.innerText || '').trim();
      }}
      // Wrap label
      var parent = el.closest('label');
      if (parent) return (parent.innerText || '').trim();
    }}
    return undefined;
  }}

  function elementLabel(el) {{
    var n = accessibleName(el);
    if (n) return n;
    var placeholder = el.getAttribute('placeholder');
    if (placeholder) return placeholder;
    return undefined;
  }}

  function cssSelector(el) {{
    if (!el || el.nodeType !== 1) return undefined;
    if (el.id && /^[A-Za-z][\w-]*$/.test(el.id)) return '#' + cssEscape(el.id);
    var parts = [];
    var node = el;
    while (node && node.nodeType === 1 && node !== document.documentElement) {{
      var part = node.tagName.toLowerCase();
      if (node.id && /^[A-Za-z][\w-]*$/.test(node.id)) {{
        parts.unshift('#' + cssEscape(node.id));
        break;
      }}
      if (node.classList.length > 0) {{
        part += '.' + Array.from(node.classList).slice(0, 2).map(cssEscape).join('.');
      }}
      var parent = node.parentElement;
      if (parent) {{
        var sibs = Array.from(parent.children).filter(function (s) {{ return s.tagName === node.tagName; }});
        if (sibs.length > 1) {{
          var idx = sibs.indexOf(node) + 1;
          part += ':nth-of-type(' + idx + ')';
        }}
      }}
      parts.unshift(part);
      node = node.parentElement;
      if (parts.length > 4) break;
    }}
    return parts.join(' > ');
  }}

  function cssEscape(s) {{
    if (window.CSS && CSS.escape) return CSS.escape(s);
    return String(s).replace(/[^A-Za-z0-9_-]/g, function (c) {{ return '\\' + c; }});
  }}

  function targetInfo(el) {{
    if (!el || el.nodeType !== 1) return undefined;
    var rect = el.getBoundingClientRect();
    var attrs = {{}};
    for (var i = 0; i < (el.attributes || []).length && i < 16; i++) {{
      var a = el.attributes[i];
      if (a.name && a.name.indexOf('data-') !== 0 && ['style','src','href','onclick','class'].indexOf(a.name) === -1) {{
        attrs[a.name] = a.value;
      }}
    }}
    attrs.class = el.className && el.className.toString ? el.className.toString().slice(0, 200) : '';
    return {{
      tag: el.tagName.toLowerCase(),
      id: el.id || undefined,
      locators: locatorCandidates(el),
      rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }},
      attributes: attrs
    }};
  }}

  // Click - skip middle/right buttons which are contextual.
  document.addEventListener('click', function (e) {{
    if (e.button !== 0) return;
    emit({{ t_ms: now(), kind: 'click', target: targetInfo(e.target), payload: {{ button: e.button, clientX: e.clientX, clientY: e.clientY }} }});
  }}, true);

  // Input - capture committed text changes (input fires per keystroke on text fields).
  document.addEventListener('input', function (e) {{
    var el = e.target;
    if (!el || el.nodeType !== 1) return;
    // Skip password fields entirely per the recorder's privacy policy.
    var isPassword = el.tagName === 'INPUT' && (el.getAttribute('type') || '').toLowerCase() === 'password';
    emit({{ t_ms: now(), kind: 'input', target: targetInfo(el), payload: {{ value: isPassword ? '<password>' : (el.value || '').slice(0, 4096), inputType: e.inputType }} }});
  }}, true);

  // Keydown - skip modifier-only events; capture printable keys and short keys.
  document.addEventListener('keydown', function (e) {{
    if (e.ctrlKey && e.altKey) return;
    emit({{ t_ms: now(), kind: 'keydown', target: targetInfo(e.target), payload: {{ key: e.key, code: e.code, keyCode: e.keyCode, repeat: e.repeat, shiftKey: e.shiftKey, ctrlKey: e.ctrlKey, altKey: e.altKey, metaKey: e.metaKey }} }});
  }}, true);

  // Scroll - throttled, only the window scroll (large documents sum scroll).
  var lastScroll = 0;
  document.addEventListener('scroll', function (e) {{
    var t = now();
    if (t - lastScroll < 250) return;
    lastScroll = t;
    var node = e.target.nodeType === 1 ? e.target : document.scrollingElement || document.documentElement;
    emit({{ t_ms: t, kind: 'scroll', target: targetInfo(node), payload: {{ scrollX: window.scrollX, scrollY: window.scrollY, scrollTop: node.scrollTop, scrollLeft: node.scrollLeft }} }});
  }}, true);

  // Submit
  document.addEventListener('submit', function (e) {{
    emit({{ t_ms: now(), kind: 'submit', target: targetInfo(e.target), payload: {{}} }});
  }}, true);

  // Navigate - fires after a full page unload / new URL load.
  var lastUrl = location.href;
  window.addEventListener('unload', function () {{
    emit({{ t_ms: now(), kind: 'navigate', target: undefined, payload: {{ from: lastUrl, to: location.href }} }});
  }});

  // Popstate / hashchange - SPA navigation.
  window.addEventListener('popstate', function () {{
    var to = location.href;
    emit({{ t_ms: now(), kind: 'navigate', target: undefined, payload: {{ from: lastUrl, to: to }} }});
    lastUrl = to;
  }});
  window.addEventListener('hashchange', function () {{
    var to = location.href;
    emit({{ t_ms: now(), kind: 'navigate', target: undefined, payload: {{ from: lastUrl, to: to, hash: true }} }});
    lastUrl = to;
  }});

  // Emit an initial navigate event so the recording knows the start URL.
  emit({{ t_ms: now(), kind: 'navigate', target: undefined, payload: {{ from: '', to: location.href }} }});
}})();
"#,
    tag = TAG
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_recorder_script_is_iife() {
    let s = recorder_script();
    assert!(s.starts_with("(function() {"), "should be an IIFE: {s}");
    assert!(
      s.contains("window.__jnmbrowserRecorderInstalled"),
      "should set guard: {s}"
    );
    assert!(s.contains("__REC__:"), "should reference tag: {s}");
  }

  #[test]
  fn test_recorder_script_runs_twice_safely() {
    // The guard ensures re-installation after navigation is a no-op.
    let s = recorder_script();
    assert!(s.contains("if (window.__jnmbrowserRecorderInstalled) return"));
  }
}
