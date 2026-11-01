#import <AppKit/AppKit.h>

@interface FixtureDelegate : NSObject <NSApplicationDelegate>
@end

@implementation FixtureDelegate
- (void)record:(NSString *)path {
    NSString *directory = [[[NSBundle mainBundle] bundlePath] stringByDeletingLastPathComponent];
    NSString *marker = [directory stringByAppendingPathComponent:@"received.txt"];
    NSError *error = nil;
    if (![path writeToFile:marker atomically:YES encoding:NSUTF8StringEncoding error:&error]) {
        fprintf(stderr, "fixture could not write the received path: %s\n", error.description.UTF8String);
        exit(1);
    }
    [NSApp terminate:nil];
}
- (void)application:(NSApplication *)application openURLs:(NSArray<NSURL *> *)urls {
    if (urls.count == 1 && urls[0].isFileURL) {
        [self record:urls[0].path];
    }
}
- (void)application:(NSApplication *)application openFiles:(NSArray<NSString *> *)files {
    if (files.count == 1) {
        [application replyToOpenOrPrint:NSApplicationDelegateReplySuccess];
        [self record:files[0]];
    }
}
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        [NSApp terminate:nil];
    });
}
@end

int main(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        FixtureDelegate *delegate = [FixtureDelegate new];
        [NSApp setDelegate:delegate];
        [NSApp run];
    }
    return 0;
}
