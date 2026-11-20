#!/usr/bin/env perl
# Inspect ELF imports without loading or executing the staged binaries.
use strict;
use warnings;
use File::Find;
use File::Spec;
use JSON::PP;
use Digest::SHA qw(sha256_hex);

$ENV{LC_ALL} = 'C';
my ($root, $ceiling, $report) = @ARGV;
die "usage: $0 APPDIR MAX_GLIBC REPORT.json\n" unless @ARGV == 3 && -d $root;
die "invalid GLIBC ceiling\n" unless $ceiling =~ /^\d+\.\d+(?:\.\d+)?$/;
$root = File::Spec->rel2abs($root);
sub compare_version {
    my @left = split /\./, $_[0];
    my @right = split /\./, $_[1];
    for my $index (0 .. 2) {
        my $cmp = ($left[$index] // 0) <=> ($right[$index] // 0);
        return $cmp if $cmp;
    }
    return 0;
}
my @paths;
find({no_chdir => 1, wanted => sub {push @paths, $File::Find::name if -f $_}}, $root);
my (@files, @violations);
my $maximum;
for my $path (sort @paths) {
    open my $file, '<:raw', $path or die "read $path: $!\n";
    read($file, my $magic, 4) == 4 or next;
    next unless $magic eq "\x7fELF";
    seek($file, 0, 0) or die "seek $path: $!\n";
    my $hash = Digest::SHA->new(256)->addfile($file)->hexdigest;
    close $file or die "close $path: $!\n";
    open my $reader, '-|', 'readelf', '--dyn-syms', '--wide', $path or die "readelf: $!\n";
    my (%required, %weak);
    while (my $line = <$reader>) {
        next unless $line =~ /^\s*\d+:\s+\S+\s+\S+\s+\S+\s+(\S+)\s+\S+\s+UND\s+(\S+)/;
        my ($binding, $symbol) = ($1, $2);
        next unless $symbol =~ /\@GLIBC_(\S+)/;
        my $version = $1;
        if ($binding eq 'WEAK') {$weak{$version} = 1; next}
        $required{$version} = 1;
    }
    close $reader or die "readelf failed for $path\n";
    # Loader-only requirements (notably DT_RELR) need not appear on an
    # undefined symbol. Read only version needs, never versions exported by a DSO.
    open $reader, '-|', 'readelf', '--version-info', '--wide', $path or die "readelf: $!\n";
    my $in_needs = 0;
    while (my $line = <$reader>) {
        $in_needs = 1 if $line =~ /^Version needs section /;
        next unless $in_needs && $line =~ /Name:\s+GLIBC_(\S+)\s+Flags:\s+(\S+)/;
        my ($version, $flags) = ($1, $2);
        if ($flags =~ /weak/i) {$weak{$version} = 1; next}
        $required{$version} = 1;
    }
    close $reader or die "readelf version needs failed for $path\n";
    my $relative = File::Spec->abs2rel($path, $root);
    my @versions = sort {
        ($a =~ /^\d/ && $b =~ /^\d/) ? compare_version($a, $b) : $a cmp $b
    } keys %required;
    my $file_max;
    for my $version (@versions) {
        # These TLS/PLT tags appeared in the 2.43 release and were backported
        # to some 2.42 packages. A numeric ceiling cannot promise a backport.
        my %abi_floor = (ABI_DT_RELR => '2.36', ABI_GNU2_TLS => '2.43',
                         ABI_GNU_TLS => '2.43', ABI_DT_X86_64_PLT => '2.43');
        my $floor = $abi_floor{$version} // $version;
        if ($floor !~ /^\d+\.\d+(?:\.\d+)?$/) {
            push @violations, "$relative imports unsupported GLIBC_$version";
            next;
        }
        $file_max = $floor if !defined($file_max) || compare_version($floor, $file_max) > 0;
        $maximum = $floor if !defined($maximum) || compare_version($floor, $maximum) > 0;
        push @violations, "$relative requires GLIBC_$version (minimum $floor) above declared $ceiling"
            if compare_version($floor, $ceiling) > 0;
    }
    push @files, {path => $relative, sha256 => $hash, required_glibc => \@versions,
                  maximum_glibc => $file_max, ignored_weak_glibc => [sort keys %weak]};
}
die "no ELF files found in $root\n" unless @files;
my $result = {schema => 1, declared_maximum_glibc => $ceiling, required_maximum_glibc => $maximum,
              elf_count => scalar(@files), files => \@files, violations => \@violations,
              passed => @violations ? JSON::PP::false : JSON::PP::true,
              scope => 'Strong dynamic imports and non-weak loader version requirements of every staged ELF; host-loaded GTK/WebKit and OS runtime behavior are not certified.'};
open my $output, '>', $report or die "write $report: $!\n";
print {$output} JSON::PP->new->canonical->pretty->encode($result);
close $output or die "close $report: $!\n";
print "Checked ", scalar(@files), " staged ELF files; maximum GLIBC ", $maximum // 'none', "; declared $ceiling\n";
if (@violations) {print STDERR "$_\n" for @violations; exit 1}
