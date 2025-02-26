use crate::tree_store::page_store::buddy_allocator::BuddyAllocatorMut;
use crate::tree_store::page_store::page_manager::{
    RegionTracker, DB_HEADER_SIZE, MAX_MAX_PAGE_ORDER, MIN_USABLE_PAGES,
};
use crate::{Error, Result};
use std::cmp::min;
use std::ops::Range;

fn div_even(value: u32, denominator: u32) -> u32 {
    assert_eq!(value % denominator, 0);
    value / denominator
}

fn round_up_to_multiple_of(value: usize, multiple: usize) -> usize {
    if value % multiple == 0 {
        value
    } else {
        value + multiple - value % multiple
    }
}

// Regions are laid out starting with the allocator state header, followed by the pages aligned
// to the next page
#[derive(Clone, Debug)]
pub(super) struct RegionLayout {
    num_pages: u32,
    // Offset where data pages start
    header_pages: u32,
    page_size: u32,
}

impl RegionLayout {
    pub(super) fn new(num_pages: u32, header_pages: u32, page_size: u32) -> Self {
        Self {
            num_pages,
            header_pages,
            page_size,
        }
    }

    fn calculate_usable_pages(space: u64, page_capacity: u32, page_size: u32) -> u32 {
        let header_pages = Self::header_pages(page_capacity, page_size) as u64;
        let page_size = page_size as u64;
        assert!(header_pages * page_size < space);
        ((space - header_pages * page_size) / page_size)
            .try_into()
            .unwrap()
    }

    fn header_pages(page_capacity: u32, page_size: u32) -> u32 {
        let mut header_size =
            BuddyAllocatorMut::required_space(page_capacity.try_into().unwrap()) as u32;

        if header_size % page_size != 0 {
            header_size += page_size - header_size % page_size;
        }

        header_size / page_size
    }

    pub(super) fn calculate(
        available_space: u64,
        desired_usable_bytes: u64,
        page_capacity: u32,
        page_size: u32,
    ) -> Option<RegionLayout> {
        let header_pages = Self::header_pages(page_capacity, page_size);
        let required_header_bytes = header_pages * page_size;
        if desired_usable_bytes / (page_size as u64) < MIN_USABLE_PAGES as u64 {
            return None;
        }
        if available_space < (required_header_bytes + (MIN_USABLE_PAGES as u32) * page_size) as u64
        {
            return None;
        }
        let max_region_size = desired_usable_bytes + required_header_bytes as u64;
        let used_space = min(max_region_size, available_space);

        let num_pages = Self::calculate_usable_pages(used_space, page_capacity, page_size);
        if num_pages < MIN_USABLE_PAGES as u32 {
            return None;
        }

        Some(RegionLayout {
            num_pages,
            header_pages,
            page_size,
        })
    }

    fn full_region_layout(page_capacity: u32, page_size: u32) -> RegionLayout {
        let max_usable_region_bytes = (page_capacity as u64) * (page_size as u64);
        let header_bytes =
            (Self::header_pages(page_capacity, page_size) as u64) * (page_size as u64);
        let max_region_size = max_usable_region_bytes + header_bytes;

        Self::calculate(
            max_region_size,
            max_usable_region_bytes,
            page_capacity,
            page_size,
        )
        .unwrap()
    }

    pub(super) fn data_section(&self) -> Range<usize> {
        let usable: usize = self.usable_bytes().try_into().unwrap();
        let header_bytes: usize = (self.header_pages * self.page_size).try_into().unwrap();
        header_bytes..(header_bytes + usable)
    }

    pub(super) fn get_header_pages(&self) -> u32 {
        self.header_pages
    }

    pub(super) fn num_pages(&self) -> u32 {
        self.num_pages
    }

    pub(super) fn len(&self) -> u64 {
        (self.header_pages as u64) * (self.page_size as u64) + self.usable_bytes()
    }

    pub(super) fn usable_bytes(&self) -> u64 {
        self.page_size as u64 * self.num_pages as u64
    }
}

#[derive(Clone)]
pub(super) struct DatabaseLayout {
    superheader_pages: u32,
    region_tracker_range: Range<usize>,
    full_region_layout: RegionLayout,
    num_full_regions: u32,
    trailing_partial_region: Option<RegionLayout>,
}

impl DatabaseLayout {
    pub(super) fn new(
        superheader_pages: u32,
        region_tracker_len: usize,
        full_regions: u32,
        full_region: RegionLayout,
        trailing_region: Option<RegionLayout>,
    ) -> Self {
        Self {
            superheader_pages,
            region_tracker_range: DB_HEADER_SIZE..(DB_HEADER_SIZE + region_tracker_len),
            full_region_layout: full_region,
            num_full_regions: full_regions,
            trailing_partial_region: trailing_region,
        }
    }

    pub(super) fn calculate(
        db_capacity: u64,
        mut desired_usable_bytes: u64,
        page_capacity: u32,
        page_size: u32,
    ) -> Result<Self> {
        desired_usable_bytes = min(desired_usable_bytes, db_capacity);
        let full_region_layout = RegionLayout::full_region_layout(page_capacity, page_size);
        let min_header_size =
            DB_HEADER_SIZE + RegionTracker::required_bytes(1, MAX_MAX_PAGE_ORDER + 1);
        let max_regions: u32 =
            ((db_capacity - (min_header_size as u64) + full_region_layout.len() - 1)
                / full_region_layout.len())
            .try_into()
            .unwrap();
        let db_header_bytes =
            DB_HEADER_SIZE + RegionTracker::required_bytes(max_regions, MAX_MAX_PAGE_ORDER + 1);
        let region_tracker_range = DB_HEADER_SIZE..db_header_bytes;
        // Pad to be page aligned
        let superheader_bytes = round_up_to_multiple_of(db_header_bytes, page_size as usize);
        if db_capacity < (superheader_bytes + MIN_USABLE_PAGES * (page_size as usize)) as u64 {
            return Err(Error::OutOfSpace);
        }
        let result = if desired_usable_bytes <= full_region_layout.usable_bytes()
            || db_capacity - (superheader_bytes as u64) <= full_region_layout.len()
        {
            // Single region layout
            let region_layout = RegionLayout::calculate(
                db_capacity - superheader_bytes as u64,
                desired_usable_bytes,
                page_capacity,
                page_size,
            )
            .ok_or(Error::OutOfSpace)?;
            DatabaseLayout {
                superheader_pages: div_even(superheader_bytes as u32, page_size),
                region_tracker_range,
                full_region_layout,
                num_full_regions: 0,
                trailing_partial_region: Some(region_layout),
            }
        } else {
            // Multi region layout
            let max_full_regions =
                (db_capacity - superheader_bytes as u64) / full_region_layout.len();
            let desired_full_regions = desired_usable_bytes / full_region_layout.usable_bytes();
            let num_full_regions = min(max_full_regions, desired_full_regions);
            let remaining_space = db_capacity
                - (superheader_bytes as u64)
                - num_full_regions * full_region_layout.len();
            let remaining_desired =
                desired_usable_bytes - num_full_regions * full_region_layout.usable_bytes();
            assert!(num_full_regions > 0);
            let trailing_region = RegionLayout::calculate(
                remaining_space,
                remaining_desired,
                page_capacity,
                page_size,
            );
            if let Some(ref region) = trailing_region {
                // All regions must have the same header size
                assert_eq!(region.header_pages, full_region_layout.header_pages);
            }
            DatabaseLayout {
                superheader_pages: div_even(superheader_bytes as u32, page_size),
                region_tracker_range,
                full_region_layout,
                num_full_regions: num_full_regions.try_into().unwrap(),
                trailing_partial_region: trailing_region,
            }
        };

        Ok(result)
    }

    pub(super) fn full_region_layout(&self) -> &RegionLayout {
        &self.full_region_layout
    }

    pub(super) fn trailing_region_layout(&self) -> Option<&RegionLayout> {
        self.trailing_partial_region.as_ref()
    }

    pub(super) fn num_full_regions(&self) -> u32 {
        self.num_full_regions
    }

    pub(super) fn num_regions(&self) -> u32 {
        if self.trailing_partial_region.is_some() {
            self.num_full_regions + 1
        } else {
            self.num_full_regions
        }
    }

    pub(super) fn len(&self) -> u64 {
        let last = self.num_regions() - 1;
        (self.region_base_address(last) as u64) + self.region_layout(last).len()
    }

    pub(super) fn usable_bytes(&self) -> u64 {
        let trailing = self
            .trailing_partial_region
            .as_ref()
            .map(RegionLayout::usable_bytes)
            .unwrap_or_default();
        (self.num_full_regions as u64) * self.full_region_layout.usable_bytes() + trailing
    }

    pub(super) fn superheader_pages(&self) -> u32 {
        self.superheader_pages
    }

    pub(super) fn superheader_bytes(&self) -> usize {
        (self.superheader_pages * self.full_region_layout.page_size) as usize
    }

    pub(super) fn region_tracker_address_range(&self) -> Range<usize> {
        self.region_tracker_range.clone()
    }

    pub(super) fn region_base_address(&self, region: u32) -> usize {
        assert!(region < self.num_regions());

        ((self.superheader_bytes() as u64) + (region as u64) * self.full_region_layout.len())
            .try_into()
            .unwrap()
    }

    pub(super) fn region_layout(&self, region: u32) -> RegionLayout {
        assert!(region < self.num_regions());
        if region == self.num_full_regions {
            self.trailing_partial_region.as_ref().unwrap().clone()
        } else {
            self.full_region_layout.clone()
        }
    }
}

#[cfg(test)]
mod test {
    use crate::tree_store::page_store::layout::RegionLayout;

    #[test]
    fn full_layout() {
        let layout = RegionLayout::full_region_layout(512, 4096);
        assert_eq!(layout.num_pages, 512);
        assert_eq!(layout.page_size, 4096);
    }
}
