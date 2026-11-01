#import <AppKit/AppKit.h>
#import <ImageIO/ImageIO.h>
#include <unistd.h>

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 7) {
            fprintf(stderr, "expected two paths and their image dimensions\n");
            return 2;
        }
        alarm(10);
        NSPasteboard *pasteboard = [NSPasteboard pasteboardWithName:NSPasteboardNameDrag];
        NSInteger changeCount = pasteboard.changeCount;
        NSArray<NSURL *> *urls = [pasteboard readObjectsForClasses:@[[NSURL class]]
            options:@{NSPasteboardURLReadingFileURLsOnlyKey: @YES}];
        if (urls.count != 2 || pasteboard.changeCount != changeCount) {
            fprintf(stderr, "the active drag pasteboard did not contain two stable file URLs\n");
            return 1;
        }
        NSMutableSet<NSString *> *remaining = [NSMutableSet set];
        for (int index = 1; index < argc; index += 3) {
            NSString *expected = [NSString stringWithUTF8String:argv[index]];
            if (!expected || [remaining containsObject:expected]) {
                fprintf(stderr, "invalid expected fixture paths\n");
                return 2;
            }
            [remaining addObject:expected];
        }
        for (NSURL *url in urls) {
            NSString *path = url.path;
            if (!url.isFileURL || ![remaining containsObject:path]) {
                fprintf(stderr, "the active drag pasteboard did not contain the fixture files\n");
                return 1;
            }
            [remaining removeObject:path];
            int index = [path isEqualToString:[NSString stringWithUTF8String:argv[1]]] ? 1 : 4;
            CGImageSourceRef image = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
            if (!image) {
                fprintf(stderr, "could not read a fixture image from the active drag\n");
                return 1;
            }
            NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(image, 0, NULL));
            CFRelease(image);
            if ([properties[(__bridge NSString *)kCGImagePropertyPixelWidth] intValue] != atoi(argv[index + 1])
                    || [properties[(__bridge NSString *)kCGImagePropertyPixelHeight] intValue] != atoi(argv[index + 2])) {
                fprintf(stderr, "a fixture image from the active drag had unexpected dimensions\n");
                return 1;
            }
        }
        if (remaining.count || pasteboard.changeCount != changeCount) {
            fprintf(stderr, "the active drag pasteboard changed during the fixture read\n");
            return 1;
        }
        puts("external process read both active drag URLs and their images");
    }
    return 0;
}
