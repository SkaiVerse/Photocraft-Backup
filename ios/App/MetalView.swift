import UIKit
import QuartzCore

/// The drawing surface. Catches the Apple Pencil, fingers, the pointer, scrolling, pinching and
/// the keyboard, and hands them to the Rust core.
///
/// Palm rejection works like Procreate's: while the Pencil touches the screen or hovers over it
/// (and for a moment after), finger touches are ignored completely.
final class MetalView: UIView, UIKeyInput, UIGestureRecognizerDelegate, UIPencilInteractionDelegate {

    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    // Hooks the view controller fills in.
    var onOpenDialog: ((_ multiple: Bool, _ pictures: Bool) -> Void)?
    var onSaveDialog: ((String) -> Void)?
    var onOpenURL: ((String) -> Void)?
    var onCopyText: ((String) -> Void)?
    /// True while a picker or alert is showing, so the view doesn't grab the keyboard back.
    var suspendFocus = false

    private var handle: UnsafeMutableRawPointer?
    private var startFailed = false
    private var link: CADisplayLink?
    private var softKeyboard = false
    private var wantsKeyboard = false
    /// The person hid the on-screen keyboard while a text field is still active.
    private var keyboardDismissed = false
    private let emptyInput = UIView(frame: .zero)
    private var activeTouch: UITouch?
    private var pointerButton: Int32 = 0
    private var held: (hid: UInt32, mods: UInt32, text: String?, start: CFTimeInterval, last: CFTimeInterval)?

    // Interface size (1.0 is the normal size of the desktop app).
    private var uiScale: Float = 1.3

    // Apple Pencil and palm rejection.
    private var pencilTouch: UITouch?
    private var pencilHovering = false
    private var pencilLastSeen: CFTimeInterval = 0
    /// How long after the Pencil leaves that fingers are still ignored.
    private let palmGrace: CFTimeInterval = 0.45
    private var eraserOn = false
    private var lastHover = CGPoint(x: 200, y: 200)
    private var hoverLogged = 0
    private var keyLogged = 0

    // Pencil samples wait a moment for iPadOS to deliver their true pressure.
    private struct PenItem {
        var phase: Int32
        var location: CGPoint
        var force: CGFloat
        var altitude: CGFloat
        var azimuth: CGVector
        var roll: CGFloat
        var mods: UInt32
        var index: NSNumber?
        var ready: Bool
        var time: CFTimeInterval
        /// A hint from iPadOS about where the tip will be. Rendered ahead of the ink, never written.
        var predicted: Bool = false
    }
    private var penQueue: [PenItem] = []
    private var smoothedPressure: Float = 0

    // Fingers: one finger is decided on by the Rust side (tap, scroll, pan or draw);
    // two or more fingers are gestures.
    private var directTouches = Set<UITouch>()
    private var ignoredTouches = Set<UITouch>()
    private var fingerTouch: UITouch?
    private var fingerLast = CGPoint.zero
    /// A fingertip resting on the screen while the Pencil draws: holds Shift (keeps proportions).
    private var constrainTouch: UITouch?

    /// True while the Pencil is touching, hovering, or has only just left the screen.
    private var pencilNear: Bool {
        pruneTouches()
        let since = CACurrentMediaTime() - pencilLastSeen
        // A hover that stopped reporting counts for a second at most.
        return pencilTouch != nil || (pencilHovering && since < 1.0) || since < palmGrace
    }

    /// Forgets touches that have ended without being reported, so nothing ever gets stuck.
    private func pruneTouches() {
        if let t = pencilTouch, t.phase == .ended || t.phase == .cancelled { pencilTouch = nil }
        directTouches = directTouches.filter { $0.phase != .ended && $0.phase != .cancelled }
        ignoredTouches = ignoredTouches.filter { $0.phase != .ended && $0.phase != .cancelled }
        if let f = fingerTouch, f.phase == .ended || f.phase == .cancelled { cancelFinger() }
        if let c = constrainTouch, c.phase == .ended || c.phase == .cancelled { releaseConstrain() }
    }

    // UITextInputTraits (no autocorrect while typing)
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = UIColor(red: 0.09, green: 0.09, blue: 0.10, alpha: 1)
        inputAssistantItem.leadingBarButtonGroups = []
        inputAssistantItem.trailingBarButtonGroups = []
        setupGestures()
        let pencil = UIPencilInteraction()
        pencil.delegate = self
        addInteraction(pencil)
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: MetalView, _) in
            view.pushTheme()
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    deinit {
        link?.invalidate()
        if let h = handle { pc_destroy(h) }
    }

    // MARK: Lifecycle

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            ensureHandle()
            if link == nil {
                let l = CADisplayLink(target: self, selector: #selector(tick))
                l.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 120, preferred: 120)
                l.add(to: .main, forMode: .common)
                link = l
            }
        } else {
            link?.invalidate()
            link = nil
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        ensureHandle()
        if let h = handle {
            let s = traitCollection.displayScale
            pc_resize(h, UInt32(max(1, bounds.width * s)), UInt32(max(1, bounds.height * s)), Float(s))
        }
    }

    private func ensureHandle() {
        guard handle == nil, !startFailed, window != nil, bounds.width > 1, bounds.height > 1 else { return }
        let s = traitCollection.displayScale
        contentScaleFactor = s
        metalLayer.contentsScale = s
        let layerPtr = Unmanaged.passUnretained(metalLayer).toOpaque()
        handle = pc_create(layerPtr, UInt32(bounds.width * s), UInt32(bounds.height * s), Float(s), makeCallbacks())
        if handle == nil {
            startFailed = true
        } else {
            pushTheme()
            uiScale = autoScale()
            pc_set_ui_scale(handle, uiScale)
        }
    }

    /// Smaller iPads get a larger interface, so buttons are easy to hit with the Pencil.
    private func autoScale() -> Float {
        let size = window?.windowScene?.screen.bounds.size ?? CGSize(width: 834, height: 1194)
        let short = Float(min(size.width, size.height))
        return max(1.15, min(1.5, 1.15 + (1100 - short) / 1000))
    }

    private func pushTheme() {
        guard let h = handle else { return }
        pc_set_dark(h, traitCollection.userInterfaceStyle == .dark ? 1 : 0)
    }

    func saveState() { if let h = handle { pc_save_state(h) } }
    func setFocused(_ focused: Bool) { if let h = handle { pc_set_focused(h, focused ? 1 : 0) } }

    @objc private func tick() {
        guard let h = handle else { return }
        if !suspendFocus && !isFirstResponder { _ = becomeFirstResponder() }
        repeatHeldKey()
        pruneTouches()
        flushPen(all: false)
        let flags = pc_frame(h)
        let wants = (flags & 2) != 0
        if wants != wantsKeyboard {
            wantsKeyboard = wants
            if !wants { keyboardDismissed = false }
        }
        let show = wantsKeyboard && !keyboardDismissed
        if show != softKeyboard {
            softKeyboard = show
            reloadInputViews()
        }
    }

    // MARK: Callbacks from Rust

    fileprivate static func from(_ p: UnsafeMutableRawPointer?) -> MetalView? {
        p.map { Unmanaged<MetalView>.fromOpaque($0).takeUnretainedValue() }
    }

    private func makeCallbacks() -> PCCallbacks {
        let me = Unmanaged.passUnretained(self).toOpaque()
        return PCCallbacks(
            user: me,
            open_dialog: { u, multiple, pictures in
                guard let v = MetalView.from(u) else { return }
                DispatchQueue.main.async { v.onOpenDialog?(multiple != 0, pictures != 0) }
            },
            save_dialog: { u, name in
                guard let v = MetalView.from(u), let name = name else { return }
                let s = String(cString: name)
                DispatchQueue.main.async { v.onSaveDialog?(s) }
            },
            write_file: { _, path, data, len in
                guard let path = path, let data = data else { return 0 }
                let bytes = Data(bytes: data, count: len)
                let name = String(cString: path)
                let ok = FileAccess.shared.write(path: name, data: bytes)
                if ok { PhotosExport.shared.finished(path: name) }
                return ok ? 1 : 0
            },
            open_url: { u, url in
                guard let v = MetalView.from(u), let url = url else { return }
                let s = String(cString: url)
                DispatchQueue.main.async { v.onOpenURL?(s) }
            },
            copy_text: { u, text in
                guard let v = MetalView.from(u), let text = text else { return }
                let s = String(cString: text)
                DispatchQueue.main.async { v.onCopyText?(s) }
            }
        )
    }

    // MARK: Results from the view controller

    /// A file arrived from outside (Files, "Open in").
    func deliverOpened(name: String, data: Data) {
        guard let h = handle else { return }
        name.withCString { n in
            data.withUnsafeBytes { raw in
                pc_open_file(h, n, raw.bindMemory(to: UInt8.self).baseAddress, data.count)
            }
        }
    }

    /// The Open sheet finished: files copied in (Photos, several files), or files opened in place
    /// (their paths), or nothing when cancelled.
    func deliverDialog(files: [(String, Data)], paths: [String], ok: Bool) {
        guard let h = handle else { return }
        for path in paths { path.withCString { pc_dialog_path(h, $0) } }
        for (name, data) in files {
            name.withCString { n in
                data.withUnsafeBytes { raw in
                    pc_dialog_file(h, n, raw.bindMemory(to: UInt8.self).baseAddress, data.count)
                }
            }
        }
        pc_dialog_end(h, ok ? 1 : 0)
    }

    /// The Save to Files sheet finished: the place chosen, or nil when cancelled.
    func deliverSaveDialog(path: String?) {
        guard let h = handle else { return }
        if let path = path { path.withCString { pc_save_dialog_end(h, $0) } } else { pc_save_dialog_end(h, nil) }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }
    override var inputView: UIView? { softKeyboard ? nil : emptyInput }

    /// The keyboard's own "hide keyboard" key lands here: keep it hidden until the next tap.
    override func resignFirstResponder() -> Bool {
        held = nil
        if softKeyboard {
            keyboardDismissed = true
            softKeyboard = false
        }
        return super.resignFirstResponder()
    }

    var hasText: Bool { true }

    func insertText(_ text: String) {
        guard let h = handle else { return }
        if text == "\n" {
            pc_key(h, 0x28, 1, 0, 0)
            pc_key(h, 0x28, 0, 0, 0)
        } else {
            text.withCString { pc_text(h, $0) }
        }
    }

    func deleteBackward() {
        guard let h = handle else { return }
        pc_key(h, 0x2A, 1, 0, 0)
        pc_key(h, 0x2A, 0, 0, 0)
    }

    private func modBits(_ f: UIKeyModifierFlags) -> UInt32 {
        var m: UInt32 = 0
        if f.contains(.shift) { m |= 1 }
        if f.contains(.control) { m |= 2 }
        if f.contains(.alternate) { m |= 4 }
        if f.contains(.command) { m |= 8 }
        return m
    }

    /// Only real typing keys type text: letters, digits, punctuation, space and the keypad.
    /// Arrows, Return, Tab, Escape, F-keys and the like are key presses, never text.
    private func typesText(_ hid: UInt32) -> Bool {
        if hid >= 0x04 && hid <= 0x27 { return true }                 // letters and digits
        if hid >= 0x2C && hid <= 0x38 { return true }                 // space and punctuation
        if hid >= 0x54 && hid <= 0x63 && hid != 0x58 { return true }  // keypad (not its Enter)
        return false
    }

    private func printable(_ s: String) -> String? {
        guard !s.isEmpty else { return nil }
        let ok = s.unicodeScalars.allSatisfy { $0.value >= 0x20 && $0.value != 0x7F && !(0xF700...0xF8FF).contains($0.value) }
        return ok ? s : nil
    }

    /// ⌘C, ⌘X and ⌘V go through the iPad's clipboard.
    private func clipboardShortcut(hid: UInt32, mods: UInt32) -> Bool {
        guard mods == 8, let h = handle else { return false }
        switch hid {
        case 0x06: pc_copy(h); return true
        case 0x1B: pc_cut(h); return true
        case 0x19:
            let s = UIPasteboard.general.string ?? ""
            s.withCString { pc_paste(h, $0) }
            return true
        default: return false
        }
    }

    private func handlePress(_ press: UIPress, down: Bool) -> Bool {
        guard let key = press.key, let h = handle else { return false }
        let hid = UInt32(key.keyCode.rawValue)
        if hid >= 0xE0 && hid <= 0xE7 { return true }   // a modifier key on its own
        let mods = modBits(key.modifierFlags)
        if down && keyLogged < 20 {
            keyLogged += 1
            pcLog("key down: usage \(hid), modifiers \(mods)")
        }
        if down {
            if clipboardShortcut(hid: hid, mods: mods) { return true }
            pc_key(h, hid, 1, 0, mods)
            var text: String? = nil
            if typesText(hid) && mods & (8 | 2) == 0 { text = printable(key.characters) }
            if let t = text { t.withCString { pc_text(h, $0) } }
            let now = CACurrentMediaTime()
            held = (hid, mods, text, now, now)
        } else {
            pc_key(h, hid, 0, 0, mods)
            if held?.hid == hid { held = nil }
        }
        return true
    }

    private func repeatHeldKey() {
        guard let k = held, let h = handle else { return }
        let now = CACurrentMediaTime()
        if now - k.start > 0.4 && now - k.last > 0.035 {
            pc_key(h, k.hid, 1, 1, k.mods)
            if let t = k.text { t.withCString { pc_text(h, $0) } }
            held?.last = now
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var rest = Set<UIPress>()
        for p in presses where !handlePress(p, down: true) { rest.insert(p) }
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var rest = Set<UIPress>()
        for p in presses where !handlePress(p, down: false) { rest.insert(p) }
        if !rest.isEmpty { super.pressesEnded(rest, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        held = nil
        super.pressesCancelled(presses, with: event)
    }

    // ⌘ shortcuts are claimed ahead of the system so ⌘Z, ⌘S, ⌘J and friends reach the app.
    private static let commandKeys: [UIKeyCommand] = {
        let letters = Array("abcdefgijklmnoprstuvxyz0123456789-=[];',./\\`").map { String($0) }
        let specials = [UIKeyCommand.inputUpArrow, UIKeyCommand.inputDownArrow, UIKeyCommand.inputLeftArrow,
                        UIKeyCommand.inputRightArrow, UIKeyCommand.inputPageUp, UIKeyCommand.inputPageDown,
                        UIKeyCommand.inputHome, UIKeyCommand.inputEnd]
        let mods: [UIKeyModifierFlags] = [.command, [.command, .shift], [.command, .alternate], [.command, .shift, .alternate]]
        var list: [UIKeyCommand] = []
        for input in letters + specials {
            for m in mods {
                let c = UIKeyCommand(input: input, modifierFlags: m, action: #selector(MetalView.commandKey(_:)))
                c.wantsPriorityOverSystemBehavior = true
                list.append(c)
            }
        }
        return list
    }()

    override var keyCommands: [UIKeyCommand]? { MetalView.commandKeys }

    private static func hid(for s: String) -> UInt32? {
        switch s {
        case UIKeyCommand.inputUpArrow: return 0x52
        case UIKeyCommand.inputDownArrow: return 0x51
        case UIKeyCommand.inputLeftArrow: return 0x50
        case UIKeyCommand.inputRightArrow: return 0x4F
        case UIKeyCommand.inputPageUp: return 0x4B
        case UIKeyCommand.inputPageDown: return 0x4E
        case UIKeyCommand.inputHome: return 0x4A
        case UIKeyCommand.inputEnd: return 0x4D
        default: break
        }
        guard s.unicodeScalars.count == 1, let u = s.lowercased().unicodeScalars.first else { return nil }
        switch u {
        case "a"..."z": return 0x04 + (u.value - 97)
        case "1"..."9": return 0x1E + (u.value - 49)
        case "0": return 0x27
        case "-": return 0x2D
        case "=": return 0x2E
        case "[": return 0x2F
        case "]": return 0x30
        case "\\": return 0x31
        case ";": return 0x33
        case "'": return 0x34
        case "`": return 0x35
        case ",": return 0x36
        case ".": return 0x37
        case "/": return 0x38
        default: return nil
        }
    }

    @objc func commandKey(_ cmd: UIKeyCommand) {
        guard let h = handle, let input = cmd.input, let hid = MetalView.hid(for: input) else { return }
        let mods = modBits(cmd.modifierFlags)
        if clipboardShortcut(hid: hid, mods: mods) { return }
        pc_key(h, hid, 1, 0, mods)
        pc_key(h, hid, 0, 0, mods)
    }

    /// A key press and release, like a shortcut typed on a keyboard.
    private func tapKey(_ hid: UInt32, mods: UInt32 = 0) {
        guard let h = handle else { return }
        pc_key(h, hid, 1, 0, mods)
        pc_key(h, hid, 0, 0, mods)
    }

    // MARK: Apple Pencil buttons

    /// Double tap on the Pencil: switch between the Brush (B) and the Eraser (E).
    func pencilInteraction(_ interaction: UIPencilInteraction, didReceiveTap tap: UIPencilInteraction.Tap) {
        pcLog("pencil: double tap")
        eraserOn.toggle()
        tapKey(eraserOn ? 0x08 : 0x05)
    }

    /// Squeeze on the Pencil Pro: opens the tool menu (a right click, which shows the brush
    /// picker for brush tools) where the Pencil is.
    func pencilInteraction(_ interaction: UIPencilInteraction, didReceiveSqueeze squeeze: UIPencilInteraction.Squeeze) {
        guard squeeze.phase == .ended, let h = handle else { return }
        pcLog("pencil: squeeze")
        pc_click(h, Float(lastHover.x), Float(lastHover.y), 1, 0)
    }

    // MARK: Gestures

    private func setupGestures() {
        let direct = [NSNumber(value: UITouch.TouchType.direct.rawValue)]

        let hover = UIHoverGestureRecognizer(target: self, action: #selector(hoverMoved(_:)))
        hover.delegate = self
        addGestureRecognizer(hover)

        let wheel = UIPanGestureRecognizer(target: self, action: #selector(pointerScroll(_:)))
        wheel.allowedScrollTypesMask = .all
        wheel.allowedTouchTypes = []
        wheel.cancelsTouchesInView = false
        wheel.delegate = self
        addGestureRecognizer(wheel)

        let twoFinger = UIPanGestureRecognizer(target: self, action: #selector(twoFingerScroll(_:)))
        twoFinger.minimumNumberOfTouches = 2
        twoFinger.maximumNumberOfTouches = 2
        twoFinger.allowedTouchTypes = direct
        twoFinger.cancelsTouchesInView = false
        twoFinger.delegate = self
        addGestureRecognizer(twoFinger)

        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        pinch.allowedTouchTypes = direct
        pinch.cancelsTouchesInView = false
        pinch.delegate = self
        addGestureRecognizer(pinch)

        // Two fingers tap: Undo. Three fingers tap: Redo.
        let twoTap = UITapGestureRecognizer(target: self, action: #selector(twoFingerTap(_:)))
        twoTap.numberOfTouchesRequired = 2
        twoTap.allowedTouchTypes = direct
        twoTap.cancelsTouchesInView = false
        twoTap.delegate = self
        addGestureRecognizer(twoTap)

        let threeTap = UITapGestureRecognizer(target: self, action: #selector(threeFingerTap(_:)))
        threeTap.numberOfTouchesRequired = 3
        threeTap.allowedTouchTypes = direct
        threeTap.cancelsTouchesInView = false
        threeTap.delegate = self
        addGestureRecognizer(threeTap)

        // Two fingers twist: rotates the picture, like turning a trackpad with two fingers.
        let rotate = UIRotationGestureRecognizer(target: self, action: #selector(rotated(_:)))
        rotate.allowedTouchTypes = direct
        rotate.cancelsTouchesInView = false
        rotate.delegate = self
        addGestureRecognizer(rotate)
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool { true }

    /// Palm rejection: no finger gesture starts while the Pencil is near.
    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        // Hovering is how the Pencil is noticed before it touches: never filter it out.
        if g is UIHoverGestureRecognizer { return true }
        if touch.type == .pencil { return false }
        if touch.type == .direct && pencilNear { return false }
        return true
    }

    @objc private func hoverMoved(_ g: UIHoverGestureRecognizer) {
        guard let h = handle else { return }
        // An Apple Pencil hovering reports a distance from the glass; a mouse or trackpad does not.
        let isPencil = g.zOffset > 0
        if hoverLogged < 8 {
            hoverLogged += 1
            pcLog("hover: state \(g.state.rawValue), pencil: \(isPencil), zOffset \(g.zOffset)")
        }
        switch g.state {
        case .began, .changed:
            let p = g.location(in: self)
            lastHover = p
            if isPencil {
                pencilHovering = true
                pencilLastSeen = CACurrentMediaTime()
                if fingerTouch != nil { cancelFinger() }
                let alt = Double(g.altitudeAngle)
                let az = g.azimuthUnitVector(in: self)
                let (tx, ty) = MetalView.tilt(altitude: alt, azimuth: az)
                var roll: Float = 0
                if #available(iOS 17.5, *) { roll = Float(g.rollAngle * 180 / CGFloat.pi) }
                pc_pen_hover(h, Float(p.x), Float(p.y), tx, ty, roll)
            } else {
                pc_pointer_moved(h, Float(p.x), Float(p.y))
            }
        case .ended, .cancelled:
            if pencilHovering {
                pencilHovering = false
                pencilLastSeen = CACurrentMediaTime()
                pc_pen_hover_end(h)
            } else {
                pc_pointer_gone(h)
            }
        default: break
        }
    }

    /// Tilt in degrees on both axes (like a browser's pen events), from the Pencil's altitude and azimuth.
    private static func tilt(altitude: Double, azimuth: CGVector) -> (Float, Float) {
        let upright = max(sin(altitude), 0.0001)
        let flat = cos(altitude)
        let tx = Float(atan2(Double(azimuth.dx) * flat, upright) * 180 / Double.pi)
        let ty = Float(atan2(Double(azimuth.dy) * flat, upright) * 180 / Double.pi)
        return (tx, ty)
    }

    @objc private func pointerScroll(_ g: UIPanGestureRecognizer) {
        guard let h = handle, g.state != .ended, g.state != .cancelled, g.state != .failed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        // A trackpad wheel needs no lead-in; a light tap must not scroll.
        if g.state == .began && t == .zero { return }
        if t != .zero { pc_scroll(h, Float(t.x), Float(t.y), modBits(g.modifierFlags)) }
    }

    @objc private func twoFingerScroll(_ g: UIPanGestureRecognizer) {
        guard let h = handle else { return }
        if g.state == .ended || g.state == .cancelled { pc_pointer_gone(h); return }
        guard g.state != .failed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        let c = g.location(in: self)
        pc_pointer_moved(h, Float(c.x), Float(c.y))
        if t != .zero { pc_scroll(h, Float(t.x), Float(t.y), 0) }
    }

    @objc private func pinched(_ g: UIPinchGestureRecognizer) {
        guard let h = handle else { return }
        switch g.state {
        case .changed:
            let p = g.location(in: self)
            pc_zoom(h, Float(g.scale), Float(p.x), Float(p.y))
            g.scale = 1
        case .ended, .cancelled:
            pc_pointer_gone(h)
        default: break
        }
    }

    @objc private func twoFingerTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended, !pencilNear else { return }
        tapKey(0x1D, mods: 8)        // ⌘Z
    }

    @objc private func threeFingerTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended, !pencilNear else { return }
        tapKey(0x1D, mods: 8 | 1)    // ⇧⌘Z
    }

    @objc private func rotated(_ g: UIRotationGestureRecognizer) {
        guard let h = handle, g.state == .changed || g.state == .ended else { return }
        pc_rotate(h, Float(g.rotation))
        g.rotation = 0
    }

    // MARK: Pencil, pointer and finger touches

    private func releaseConstrain() {
        constrainTouch = nil
        if let h = handle { pc_set_touch_shift(h, 0) }
    }

    /// Drops a finger that was acting as the pointer (a second finger or the Pencil arrived).
    private func cancelFinger() {
        if fingerTouch != nil, let h = handle {
            pc_finger(h, 3, Float(fingerLast.x), Float(fingerLast.y), 0)
        }
        fingerTouch = nil
    }

    /// Adds a Pencil sample. It is sent once its true pressure has arrived (a few milliseconds).
    private func queuePen(_ touch: UITouch, phase: Int32, event: UIEvent?, mods: UInt32) {
        var samples: [UITouch] = [touch]
        if phase == 1, let more = event?.coalescedTouches(for: touch), !more.isEmpty { samples = more }
        for c in samples {
            let maxForce = c.maximumPossibleForce
            let waiting = c.estimationUpdateIndex != nil && c.estimatedPropertiesExpectingUpdates.contains(.force)
            var roll: CGFloat = 0
            if #available(iOS 17.5, *) { roll = c.rollAngle }
            penQueue.append(PenItem(
                phase: phase,
                location: c.preciseLocation(in: self),
                force: maxForce > 0 ? c.force : 1,
                altitude: c.altitudeAngle,
                azimuth: c.azimuthUnitVector(in: self),
                roll: roll,
                mods: mods,
                index: waiting ? c.estimationUpdateIndex : nil,
                ready: !waiting,
                time: CACurrentMediaTime()))
        }
        // iPadOS also predicts where the tip will be before the next real touch arrives. Feed
        // those into the queue too: they are rendered ahead of the ink, so a fast stroke keeps
        // the tip in front of the line. When the real sample lands it replaces them.
        if phase == 1,
           let predicted = event?.predictedTouches(for: touch),
           !predicted.isEmpty
        {
            for c in predicted {
                var roll: CGFloat = 0
                if #available(iOS 17.5, *) { roll = c.rollAngle }
                penQueue.append(PenItem(
                    phase: 1,
                    location: c.preciseLocation(in: self),
                    force: c.maximumPossibleForce > 0 ? c.force : 1,
                    altitude: c.altitudeAngle,
                    azimuth: c.azimuthUnitVector(in: self),
                    roll: roll,
                    mods: mods,
                    index: nil,
                    ready: true,
                    time: CACurrentMediaTime(),
                    predicted: true))
            }
        }
        flushPen(all: false)
    }

    /// iPadOS refines the Pencil's pressure a few milliseconds after the touch. Matching the
    /// refined value to its sample gives an even, natural stroke.
    override func touchesEstimatedPropertiesUpdated(_ touches: Set<UITouch>) {
        for t in touches where t.type == .pencil {
            guard let index = t.estimationUpdateIndex,
                  let i = penQueue.firstIndex(where: { $0.index == index }) else { continue }
            penQueue[i].force = t.force
            penQueue[i].altitude = t.altitudeAngle
            if !t.estimatedPropertiesExpectingUpdates.contains(.force) { penQueue[i].ready = true }
            // The real sample has landed: drop the predictions it superseded, so the ink never
            // draws the same point twice. (Predicted samples sit behind it in the queue.)
            if i + 1 < penQueue.count { penQueue.removeSubrange(i + 1 ..< penQueue.count) }
        }
        flushPen(all: false)
    }

    private func flushPen(all: Bool) {
        let now = CACurrentMediaTime()
        // A sample waits at most about one frame for its refined pressure: the ink keeps up with
        // the tip. Predicted samples are already ready, so they go out immediately and stay ahead
        // of the ink even when a real sample is still waiting for its pressure.
        while let first = penQueue.first, first.predicted || first.ready || all || now - first.time > 0.012 {
            penQueue.removeFirst()
            sendPen(first)
        }
    }

    private func sendPen(_ item: PenItem) {
        guard let h = handle else { return }
        // The Pencil's force (1.0 is an average touch). Predicted samples are real strokes drawn
        // ahead of the ink; they are dropped when the sample they predict lands, so a real
        // sample landing later cannot double-draw.
        let force = item.predicted ? Float(max(0, min(4.5, item.force))) : smoothForce(item.force, item.phase)
        let (tx, ty) = MetalView.tilt(altitude: Double(item.altitude), azimuth: item.azimuth)
        pc_pen(h, Float(item.location.x), Float(item.location.y), item.phase, force, tx, ty,
               Float(item.roll * 180 / CGFloat.pi), item.mods)
    }

    /// Pressure only (never the position): a little EMA evens out the response between the
    /// raw force and iPadOS's refined value.
    private func smoothForce(_ force: CGFloat, _ phase: Int32) -> Float {
        let f = Float(max(0, min(4.5, force)))
        if phase == 0 { smoothedPressure = f } else { smoothedPressure = smoothedPressure * 0.5 + f * 0.5 }
        return smoothedPressure
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let h = handle else { return }
        pruneTouches()
        keyboardDismissed = false
        let mods = modBits(event?.modifierFlags ?? [])
        for t in touches {
            let p = t.location(in: self)
            switch t.type {
            case .pencil:
                // Anything already resting on the screen is a palm.
                for d in directTouches { ignoredTouches.insert(d) }
                directTouches.removeAll()
                cancelFinger()
                pencilTouch = t
                pencilLastSeen = CACurrentMediaTime()
                penQueue.removeAll()
                queuePen(t, phase: 0, event: event, mods: mods)
            case .indirectPointer:
                pointerButton = (event?.buttonMask.contains(.secondary) ?? false) ? 1 : 0
                pc_pointer_moved(h, Float(p.x), Float(p.y))
                pc_pointer_button(h, Float(p.x), Float(p.y), pointerButton, 1, mods)
            default:
                // One fingertip (not a palm) on the screen while the Pencil is drawing or
                // resizing: keeps the proportions, like holding Shift.
                if pencilTouch != nil && constrainTouch == nil && t.majorRadius < 20 {
                    constrainTouch = t
                    pc_set_touch_shift(h, 1)
                    continue
                }
                if pencilNear {
                    ignoredTouches.insert(t)
                    continue
                }
                directTouches.insert(t)
                if directTouches.count == 1 {
                    fingerTouch = t
                    fingerLast = p
                    pc_finger(h, 0, Float(p.x), Float(p.y), mods)
                } else {
                    // A second finger means scrolling, pinching or a tap gesture, never a press.
                    cancelFinger()
                }
            }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let h = handle else { return }
        let mods = modBits(event?.modifierFlags ?? [])
        for t in touches {
            let p = t.location(in: self)
            switch t.type {
            case .pencil:
                if t === pencilTouch { queuePen(t, phase: 1, event: event, mods: mods) }
            case .indirectPointer:
                pc_pointer_moved(h, Float(p.x), Float(p.y))
            default:
                guard t === fingerTouch else { continue }
                fingerLast = p
                pc_finger(h, 1, Float(p.x), Float(p.y), mods)
            }
        }
    }

    private func finish(_ touches: Set<UITouch>, with event: UIEvent?, cancelled: Bool) {
        guard let h = handle else { return }
        let mods = modBits(event?.modifierFlags ?? [])
        for t in touches {
            let p = t.location(in: self)
            ignoredTouches.remove(t)
            switch t.type {
            case .pencil:
                if t === pencilTouch {
                    queuePen(t, phase: cancelled ? 3 : 2, event: event, mods: mods)
                    pencilTouch = nil
                    pencilLastSeen = CACurrentMediaTime()
                }
            case .indirectPointer:
                pc_pointer_button(h, Float(p.x), Float(p.y), pointerButton, 0, mods)
            default:
                if t === constrainTouch {
                    releaseConstrain()
                    continue
                }
                directTouches.remove(t)
                guard t === fingerTouch else { continue }
                pc_finger(h, cancelled ? 3 : 2, Float(p.x), Float(p.y), mods)
                fingerTouch = nil
            }
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { finish(touches, with: event, cancelled: false) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { finish(touches, with: event, cancelled: true) }
}
