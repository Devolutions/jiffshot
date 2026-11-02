#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>

static int report_owner(const char *x, const char *y) {
    [NSApplication sharedApplication];
    CGFloat mainHeight = CGDisplayBounds(CGMainDisplayID()).size.height;
    NSPoint point = NSMakePoint(strtod(x, NULL), mainHeight - strtod(y, NULL));
    NSInteger window = [NSWindow windowNumberAtPoint:point belowWindowWithWindowNumber:0];
    printf("%ld\n", (long)window);
    return 0;
}

static int report_cursor(void) {
    CGEventRef event = CGEventCreate(NULL);
    if (!event) {
        fprintf(stderr, "could not query the pointer\n");
        return 1;
    }
    CGPoint point = CGEventGetLocation(event);
    CFRelease(event);
    printf("%.17g %.17g\n", point.x, point.y);
    return 0;
}

static int move_cursor(const char *x, const char *y) {
    CGError error = CGWarpMouseCursorPosition(CGPointMake(strtod(x, NULL), strtod(y, NULL)));
    if (error != kCGErrorSuccess) {
        fprintf(stderr, "could not move the fixture pointer: %d\n", error);
        return 1;
    }
    return 0;
}

@interface InputView : NSTextView
@property(nonatomic, copy) NSString *statePath;
@property(nonatomic, strong) NSMutableArray<NSDictionary *> *events;
- (void)record:(NSEvent *)event name:(NSString *)name;
- (void)activateFixtureButton:(NSButton *)button;
@end

@implementation InputView
- (void)record:(NSEvent *)event name:(NSString *)name {
    NSMutableDictionary *entry = [@{
        @"name": name,
        @"modifiers": @((unsigned long long)event.modifierFlags),
    } mutableCopy];
    if ([name hasSuffix:@"Down"] || [name hasSuffix:@"Up"] || [name hasSuffix:@"Drag"]) {
        if (event.type != NSEventTypeKeyDown && event.type != NSEventTypeKeyUp) {
            entry[@"clicks"] = @(event.clickCount);
        }
    }
    if (event.type == NSEventTypeScrollWheel) {
        entry[@"dx"] = @(event.scrollingDeltaX);
        entry[@"dy"] = @(event.scrollingDeltaY);
    }
    if (event.type == NSEventTypeKeyDown || event.type == NSEventTypeKeyUp ||
        event.type == NSEventTypeFlagsChanged) {
        entry[@"key"] = @(event.keyCode);
        if (event.type != NSEventTypeFlagsChanged) {
            entry[@"characters"] = event.characters ?: @"";
        }
    }
    [self.events addObject:entry];
    NSDictionary *state = @{@"events": self.events, @"text": self.string ?: @""};
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:state options:0 error:&error];
    if (!data || ![data writeToFile:self.statePath options:NSDataWritingAtomic error:&error]) {
        fprintf(stderr, "could not publish input fixture state: %s\n", error.description.UTF8String);
        [NSApp terminate:nil];
    }
}

- (void)mouseMoved:(NSEvent *)event { [self record:event name:@"move"]; }
- (void)mouseDown:(NSEvent *)event {
    [self.window makeFirstResponder:self];
    [self record:event name:@"leftDown"];
}
- (void)mouseUp:(NSEvent *)event { [self record:event name:@"leftUp"]; }
- (void)mouseDragged:(NSEvent *)event { [self record:event name:@"leftDrag"]; }
- (void)rightMouseDown:(NSEvent *)event { [self record:event name:@"rightDown"]; }
- (void)rightMouseUp:(NSEvent *)event { [self record:event name:@"rightUp"]; }
- (void)rightMouseDragged:(NSEvent *)event { [self record:event name:@"rightDrag"]; }
- (void)otherMouseDown:(NSEvent *)event { [self record:event name:@"otherDown"]; }
- (void)otherMouseUp:(NSEvent *)event { [self record:event name:@"otherUp"]; }
- (void)otherMouseDragged:(NSEvent *)event { [self record:event name:@"otherDrag"]; }
- (void)scrollWheel:(NSEvent *)event { [self record:event name:@"scroll"]; }
- (void)flagsChanged:(NSEvent *)event { [self record:event name:@"flags"]; }
- (void)keyDown:(NSEvent *)event {
    [super keyDown:event];
    [self record:event name:@"keyDown"];
}
- (void)keyUp:(NSEvent *)event { [self record:event name:@"keyUp"]; }
- (void)activateFixtureButton:(NSButton *)button {
    [self record:NSApp.currentEvent name:[@"button-" stringByAppendingString:button.identifier]];
}
@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc == 4 && strcmp(argv[1], "--owner-at") == 0) {
            return report_owner(argv[2], argv[3]);
        }
        if (argc == 2 && strcmp(argv[1], "--cursor") == 0) {
            return report_cursor();
        }
        if (argc == 4 && strcmp(argv[1], "--warp") == 0) {
            return move_cursor(argv[2], argv[3]);
        }
        if (argc != 3) {
            fprintf(stderr, "expected fixture information and state paths\n");
            return 2;
        }
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        NSScreen *screen = NSScreen.mainScreen;
        if (!screen) {
            fprintf(stderr, "no main display\n");
            return 1;
        }
        NSRect visible = screen.visibleFrame;
        NSRect content = NSMakeRect(NSMinX(visible) + 35, NSMinY(visible) + 35, 260, 180);
        NSWindow *window = [[NSWindow alloc] initWithContentRect:content
            styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
        window.releasedWhenClosed = NO;
        window.title = @"Jiffshot Input Fixture";
        window.acceptsMouseMovedEvents = YES;
        InputView *view = [[InputView alloc] initWithFrame:NSMakeRect(0, 0, 260, 180)];
        view.statePath = [NSString stringWithUTF8String:argv[2]];
        view.events = [NSMutableArray array];
        view.richText = NO;
        view.font = [NSFont systemFontOfSize:22];
        view.automaticQuoteSubstitutionEnabled = NO;
        view.automaticDashSubstitutionEnabled = NO;
        NSView *container = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 260, 180)];
        [container addSubview:view];
        for (NSArray<NSString *> *spec in @[@[@"Synthetic Save", @"save"], @[@"Synthetic Save As", @"save-as"]]) {
            NSButton *button = [NSButton buttonWithTitle:spec[0] target:view action:@selector(activateFixtureButton:)];
            button.identifier = spec[1];
            button.frame = NSMakeRect([spec[1] isEqualToString:@"save"] ? 5 : 130, 5, 120, 26);
            [container addSubview:button];
        }
        NSSecureTextField *secure = [[NSSecureTextField alloc] initWithFrame:NSMakeRect(5, 35, 70, 24)];
        secure.stringValue = @"FIXTURE_PRIVATE_VALUE_9182";
        [container addSubview:secure];
        window.contentView = container;
        [app activateIgnoringOtherApps:YES];
        [window makeKeyAndOrderFront:nil];
        [window makeFirstResponder:view];
        [window display];
        NSDictionary *bounds = @{@"Width": @(window.frame.size.width), @"Height": @(window.frame.size.height)};
        NSDictionary *info = @{
            @"pid": @(NSProcessInfo.processInfo.processIdentifier),
            @"window": @(window.windowNumber),
            @"bounds": bounds,
        };
        NSError *error = nil;
        NSData *data = [NSJSONSerialization dataWithJSONObject:info options:0 error:&error];
        if (!data || ![data writeToFile:[NSString stringWithUTF8String:argv[1]] options:NSDataWritingAtomic error:&error]) {
            fprintf(stderr, "could not publish input fixture details: %s\n", error.description.UTF8String);
            return 1;
        }
        [app run];
    }
    return 0;
}
